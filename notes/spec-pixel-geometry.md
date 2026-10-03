# Spec: one pane pixel extent, one pixel mouse decision

Implementation spec for three hunt entries that share one fix:

- CON-022, "Is pixel mouse eligible, and who decides?"
  (`notes/hunt-consolidations.md`)
- CON-023, "How big is a pane in pixels?" (`notes/hunt-consolidations.md`)
- TYP-048, "Some cell and pixel extents still use zero for unknown"
  (`notes/hunt-types.md`)

Written against `reference/technical-implementation-spec.md`. Every entry
was checked against the code on the date of writing; the survey below is
what the code does, not what the entries say. Where they differ, the
Findings section says so.

Sibling specs written at the same time: `notes/spec-app-loop.md` (B: `App`
and the server loop, `render.rs` included apart from
`stream_host_mouse_capture_mode`) and `notes/spec-data-model.md` (A:
`AppState` and the workspace tree). This spec does not redesign their parts.
The last section lists what it assumes of them and what it offers them.

## 1. Contracts inventoried

- `docs/` does not exist.
- `reference/` holds only `technical-implementation-spec.md`, the contract
  this spec is written against.
- `AGENTS.md` binds this work in five places:
  - Crate layering. `shepr-core` sits below `shepr-term`, which sits below
    `shepr-vt`, `shepr-protocol` and everything above. Every type here is
    placed so no lower crate names a higher one.
  - The `shepr-term` description lists "the host's observed theme and cell
    size". Landing 1 moves the host cell out of `shepr-term` (into
    `shepr-core`, beside `HostGeometry`, which already holds it), so that
    line changes. Landing 3 puts pixel mouse eligibility into `shepr-term`,
    so the same line gains it. Both edits are bricks below.
  - Wire rules: no `skip_serializing_if`, `flatten`, `untagged` or tagged
    enums. The new wire enums (`ReportedCell`, `HostMouseCapture`, the
    reshaped `ClientMousePosition`) use serde's default external tagging,
    which the codec already carries for `ClientMousePosition` today.
  - "No wire compatibility obligations": client and server are one build,
    so the wire reshapes land in one step with no fixtures to keep.
  - The key encoding note (full kitty report-all and modifyOtherKeys level 1
    and 2 deliberately not implemented). Nothing here touches key encoding.
    The mouse encoder move in Landing 2 moves only mouse code.
- "Presentation is per client" (AGENTS.md, Principles). After this work
  the pane surface no longer depends on the viewing client's cell size, so
  the surface render key loses its cell axes and two clients with the same
  area and workspace share one render. That is in line with the principle
  (each client still gets its own surface, location and baseline); no
  wording changes.

## 2. Decisions

### 2.1 What a non-source client is told to capture

A workspace's PTYs are sized for one client, the geometry source
(`workspace_geometry_source` in
`crates/shepr-server/src/server/headless/client_views.rs`). The child sees
that client's cell size times the pane grid as its pixel extent. Today
every client is published its own product (`inner_rect` times its own cell)
and told to capture pixels whenever its host is exact and the focused pane
asks for mode 1016. A client whose cell differs from the source's then sends
pixels that the admission gate always refuses, because its published extent
differs from the runtime's.

Decision: every client is published the pane's one extent, the one the
child was told, and a client is told to capture pixels when its own host is
exact, the focused pane asks for 1016 and has a known extent, and the client
presents that pane at the PTY's grid. That holds for the source and for any
other client of the same surface size, whatever its cell size. Such a
client maps each host pixel into the pane's extent: the client already
rescales the offset inside its own host cell onto the matching cell of the
target extent (`map_axis_within_cell` in
`crates/shepr-termio/src/input/mouse.rs`), so a 10x20 host cell lands
correctly in a pane whose child believes in 8x16 cells. The child gets
coordinates in the geometry it was told, at the precision the client's own
cell allows.

A client that presents the pane at another grid (a different surface size,
or the pane clipped during a resize) is told cells. Pixel positions cannot
be mapped there: cell `n` on its screen is not cell `n` of the child.

### 2.2 What unknown means

Unknown becomes a variant or a `None`, never a zero:

- A host cell is `HostCell::{Unknown, Estimated(CellPx), Exact(CellPx)}`.
  The exactness flag that travels beside the cell today
  (`HostCellGeometry.exact`, `TerminalGeometry.pixel_mouse`,
  `ClientConnection.pixel_mouse`) is the variant. `Exact` is the one
  per-connection pixel mode value.
- A pane's pixel extent is `Option<PanePixelExtent>`, nonzero by type.
- `CellPx` is a bounded, validated cell with private fields. A raw nonzero
  report a peer or host sent, possibly oversized, is a separate
  `CellReport`, so refusal diagnostics keep the raw value without making an
  unvalidated `CellPx`.
- Zero stays only where an external ABI spells unknown as zero: the PTY
  winsize (`TIOCSWINSZ`) in `crates/shepr-pty/src/fd.rs`, and the raw host
  readings (ioctl pixel fields, XTWINOPS replies) at the one constructor
  that reads them, `HostCell::from_host`.

## 3. Survey of the current code

### 3.1 Pane pixel extent (CON-023)

Three computations:

1. `shepr-core` `PaneGeometry::text_area_px()` (`crates/shepr-core/src/geometry.rs`):
   `cols * cell.width` and `rows * cell.height`, each clamped to `u16::MAX`,
   `None` without a cell. Readers:
   - `shepr-pty` `fd.rs` sets the winsize from it, `(0, 0)` when `None`.
   - `shepr-vt` `handler.rs`: `in_band_size_report` (mode 2048) and
     `text_area_pixels_report` (`CSI 14 t`), both through
     `geometry_for_terminal(cols, rows, self.cell)`.
   - `shepr-vt` `Terminal::text_area_px`, `width_px`, `height_px`
     (`crates/shepr-vt/src/lib.rs`); the last two return `0` for unknown
     ("Mouse encoding currently consumes zero-valued axes").
   - `shepr-mux` `PaneRuntime::pixel_size()` (`pane/runtime/input.rs`) wraps
     it from `current_size` as `PanePixelSize { pub width: u32, pub height:
     u32 }` (`pane/runtime.rs`).
2. `shepr-server` `pane_surface.rs::render_pane_surface` publishes
   `inner_rect` times the rendering client's `HostCellSize`, unclamped,
   `(0, 0)` when unknown, as `PaneSurfacePane.pixel_width/height`
   (`crates/shepr-protocol/src/surface.rs`). The render key
   (`pane_surface_render_key` in `headless/render.rs`) therefore carries the
   cell (`PaneSurfaceRenderKey = (Option<WorkspaceId>, u16, u16, u32, u32)`).
3. `shepr-mux` `PaneTerminal::encode_mouse_event_with_modes`
   (`pane/terminal/input.rs`) locks the core to derive its own `cell_pitch`
   from `terminal.width_px() / cols`.

`CSI 16 t` (cell size, `ScanEvent::CellSizeQuery` in `shepr-vt/src/lib.rs`)
answers the raw `self.cell`, not derived from the clamped extent.

### 3.2 Pixel mouse deciders (CON-022)

Client side:

- `crates/shepr-client/src/terminal_setup.rs`: `effective_sgr_pixel_mouse(enabled,
  requested, exact_geometry)` decides whether the host goes into 1016;
  `host_mouse_capture_update` and `HostMouseMode::apply` call it with
  `state.reported_geometry.exact()` (from `client_loop.rs`,
  `shell_runtime.rs`, `dispatch.rs`).
- `crates/shepr-client/src/input.rs` `classify_unix_input` attaches
  `HostPixels` to SGR reports when the probe says the host is in pixel mode.
- `crates/shepr-client/src/shell/input/mouse.rs` `pane_mouse_position`:
  pixels when `hit.sgr_pixel_mouse && hit.pixel_width > 0 && hit.pixel_height
  > 0` and `HostPixels::pane_position` maps; `push_pane_mouse_event` and the
  gesture release in `shell/input/mod.rs` (`release_input_leases`) attach a
  `ClientMouseGeometry { cols, rows, width_px, height_px }` built from the hit.
- `crates/shepr-client/src/shell/state.rs` `PaneHit::from_wire` zeroes the
  pixel fields when the pane is clipped.

Server side:

- `headless/render.rs` `stream_host_mouse_capture_mode`: `sgr_pixels =
  client.pixel_mouse && focused runtime's sgr_pixel_mouse_enabled()`; no
  pane extent, no presentation check. Sent through
  `ClientOutbox::tell_mouse_capture(enabled, sgr_pixels)` (`server/outbox.rs`),
  which dedupes with `Told.mouse_capture: Option<(bool, bool)>`.
- `headless.rs` `ServerEvent::ShellPaneInput` arm: `pixel_mouse =
  client.pixel_mouse && client.outbox.told_sgr_pixels()`. The dedupe memo
  is an authority. `forget_presentation()` clears it on
  `ShellReplayHostEffects` and on surface activation
  (`headless/surface_interest.rs`, which deliberately does not re-tell until
  the client asks for the replay), so in that window every pixel report is
  downgraded.
- `server/pane_input.rs` `downgrade_ineligible_pixel_mouse(events,
  pixel_mouse, runtime_size, runtime_pixels)`: the real gate. A `Pixels`
  report survives only if the bool holds, the echoed `ClientMouseGeometry`
  grid and pixels equal the runtime's `grid_size()` and `pixel_size()`, and
  the coordinates are in range. Otherwise it becomes `Cell`.
- `server/pane_input.rs` `apply_mouse` reads `InputModes` and turns
  `Pixels` into `Cell` again when the pane is not in 1016.
- mux `encode_mouse_event_with_modes`: `Cell` under 1016 becomes the cell's
  top-left pixel through `cell_pitch`; `Pixels` without 1016 is divided
  back through `cell_pitch` (dead in practice, `apply_mouse` already
  converted it).

The pane mode bit is read four ways, not three:

- `shepr-vt` stores it as `modes.sgr_pixels_mouse` and copies it twice into
  `InputModes` (`sgr_pixel_mouse` and `mouse_protocol.pixels_requested`).
- `shepr-mux` `PaneTerminal::sgr_pixel_mouse_enabled` (`terminal/backend.rs`,
  `mode_enabled(DecMode::MouseSgrPixels)`) behind `PaneRead::sgr_pixel_mouse_enabled`
  (`runtime/read.rs`), read by the capture stream and by
  `PaneSurfaceMetadata::from_runtime`.
- The dirty snapshot (`collect_dirty_patch_snapshot` in `terminal/backend.rs`)
  reads `mode_get(DecMode::MouseSgrPixels)` into
  `TerminalDirtyPatchSnapshot.sgr_pixel_mouse` (`terminal.rs`), copied into
  `PaneSurfaceMetadata::from_dirty_snapshot`.
- The wire copy `PaneSurfacePane.sgr_pixel_mouse`, into
  `PaneHit.sgr_pixel_mouse`.

### 3.3 Zero for unknown (TYP-048)

- `shepr-term` `HostCellSize { pub width_px: u32, pub height_px: u32 }`
  (`crates/shepr-term/src/host.rs`), `Default` zero, `cell()` re-validates
  through `HostCellGeometry::from_wire`, `or_default()`. Users are all in
  `shepr-server`: `ClientConnection.cell_size` and `RenderTarget.cell_size`
  (`server/clients.rs`), `ClientViewKey.cell_size` (`headless.rs`),
  `SpawnGeometry.cell_size` (`app/state.rs`), `headless_spawn_geometry`
  (`app/creation.rs`), `ui::resize_surface` and `ui::panes::resize_pane_infos`,
  the render key and `SurfaceBoundary::render` (`headless/render.rs`),
  `render_pane_surface`, and test literals.
- `shepr-core` `HostCellGeometry { cell: Option<CellPx>, exact: bool }` with
  `from_host` (clamps oversized, clears exact) and `from_wire` (refuses
  oversized); `width()`/`height()` return 0 for unknown. Aliased as
  `shepr_protocol::ProtocolCellSize`.
- `HostGeometry::new(cols, rows, width, height, exact)`, `cell_width()`,
  `cell_height()` (0 for unknown; used in the connect log line in
  `headless.rs` and tests).
- `PaneGeometry::new(cols, rows, width, height)` (zero means no cell),
  `cell_width()`/`cell_height()` (0 for unknown, no production caller).
- `CellPx { pub width: NonZeroU32, pub height: NonZeroU32 }`: any nonzero
  value, bound checked separately by `within_host_limit`. Constructed raw by
  `shepr-termio` `parse_host_cell_size_report`, client `ioctl_cell_size`,
  `AtomicCellSize`, and the protocol wire.
- `shepr-protocol` `TerminalGeometry { grid, cell: Option<CellPx>,
  pixel_mouse: bool }` with a `TryFrom` refusing `pixel_mouse` without a
  cell; `width()`/`height()` return 0 for unknown and feed
  `client_shell_geometry_error` in `server/client_transport.rs`.
- `ClientConnection.pixel_mouse: bool` beside `cell_size`.
- `ClientMouseGeometry { cols: u16, rows: u16, width_px: u32, height_px: u32 }`
  (`crates/shepr-protocol/src/input.rs`), an `Option` beside
  `ClientMousePosition` in `ClientPaneInputEvent::Mouse` whose presence must
  agree with the position variant.
- `PaneSurfacePane.pixel_width/pixel_height: u32`; `PaneHit.pixel_width/height`.
- mux `PanePixelSize` with public `u32` fields.
- `HostPixels::pane_position(inner, child_width_px: u32, child_height_px: u32)`
  (`crates/shepr-termio/src/input/mouse.rs`).

## 4. Target

### 4.1 shepr-core (`crates/shepr-core/src/geometry.rs`)

```rust
/// A raw nonzero cell size as a host or peer reported it, possibly above
/// `CellPx::MAX_DIMENSION`. Kept raw so a refusal can name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellReport {
    pub width: NonZeroU32,
    pub height: NonZeroU32,
}

impl CellReport {
    pub fn new(width: u32, height: u32) -> Option<Self>;
    /// The usable cell, `None` when an axis is above the bound.
    pub fn cell(self) -> Option<CellPx>;
}

/// A usable cell size: nonzero and within `MAX_DIMENSION` on both axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "CellReport", into = "CellReport")]
pub struct CellPx {
    width: NonZeroU32,
    height: NonZeroU32,
}

impl CellPx {
    pub const MAX_DIMENSION: u32 = crate::limits::MAX_HOST_CELL_PX;
    /// `None` for a zero or oversized axis.
    pub fn new(width: u32, height: u32) -> Option<Self>;
    pub fn width(self) -> NonZeroU32;
    pub fn height(self) -> NonZeroU32;
}
// TryFrom<CellReport> for CellPx (error: the report), From<CellPx> for CellReport.
// `within_host_limit` is deleted: `new` and `CellReport::cell` own the bound.

/// The host terminal's cell as one observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostCell {
    Unknown,
    /// A cell the host did not measure exactly: a guess, an XTWINOPS reply
    /// without an ioctl extent, or a clamped report.
    Estimated(CellPx),
    /// Measured from one coherent ioctl; pixel mouse is possible.
    Exact(CellPx),
}

impl HostCell {
    /// A host reading: a zero axis is unknown, an oversized one is clamped
    /// to `CellPx::MAX_DIMENSION` and never exact. The one place a zero
    /// from the host is interpreted.
    pub fn from_host(width: u32, height: u32, exact: bool) -> Self;
    /// The same for a nonzero raw report.
    pub fn from_report(report: CellReport, exact: bool) -> Self;
    pub fn cell(self) -> Option<CellPx>;
    pub fn is_exact(self) -> bool;
    /// What a connection keeps after a newer observation: an unknown newer
    /// observation keeps this cell, demoted to an estimate.
    pub fn refreshed_by(self, next: Self) -> Self;
}

/// Physical host geometry (unchanged role).
pub struct HostGeometry { grid: GridSize, cell: HostCell }

impl HostGeometry {
    pub fn new(grid: GridSize, cell: HostCell) -> Self;
    pub fn with_grid(self, grid: GridSize) -> Self;
    pub fn grid(self) -> GridSize;
    pub fn cell(self) -> HostCell;
    pub fn cols(self) -> u16;
    pub fn rows(self) -> u16;
}
// Deleted: HostCellGeometry (whole type), HostGeometry::with_cell,
// cell_geometry, exact, cell_width, cell_height.

/// A pane's text area in pixels as its child was told it (winsize,
/// `CSI 14 t`, mode 2048), with the grid it belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePixelExtent {
    grid: GridSize,
    width: NonZeroU16,
    height: NonZeroU16,
}

impl PanePixelExtent {
    pub fn new(grid: GridSize, width: u16, height: u16) -> Option<Self>;
    pub fn grid(self) -> GridSize;
    pub fn width(self) -> NonZeroU16;
    pub fn height(self) -> NonZeroU16;
    /// The integer cell pitch a child derives from this extent:
    /// `(width / cols).max(1)`, `(height / rows).max(1)`.
    pub fn cell_pitch(self) -> (NonZeroU32, NonZeroU32);
    /// Whether a 1-based pixel lies inside the extent.
    pub fn contains(self, x: u32, y: u32) -> bool;
    /// The 1-based top-left pixel of a 0-based cell, through `cell_pitch`.
    pub fn cell_origin(self, column: u16, row: u16) -> (u32, u32);
}

impl PaneGeometry {
    pub fn with_cell(cols: u16, rows: u16, cell: Option<CellPx>) -> Self; // unchanged
    /// A pane with no known cell size.
    pub fn cells_only(cols: u16, rows: u16) -> Self;
    /// The extent `TIOCSWINSZ` can carry: each axis clamped to `u16::MAX`.
    /// `None` without a cell.
    pub fn pixel_extent(self) -> Option<PanePixelExtent>;
}
// Deleted: PaneGeometry::new(cols, rows, width, height), cell_width,
// cell_height, text_area_px.
```

`PaneGeometry` is the one owner of a pane's extent. Everything else reads
`pixel_extent()` of the geometry the pane was last resized to.

### 4.2 shepr-term

`crates/shepr-term/src/host.rs`: `HostCellSize` is deleted. The module keeps
the theme types.

`crates/shepr-term/src/mouse.rs` gains the pixel mouse vocabulary and the
one decision, next to `MouseProtocol`:

```rust
/// A pointer position delivered to a pane. A pixel position keeps the cell
/// it lies in, so a pane that is not in mode 1016 gets that cell.
pub enum Position {
    Cell { column: u16, row: u16 },
    Pixels { column: u16, row: u16, x: u32, y: u32 },
}

/// The mouse protocol the child selected. (`pixels_requested` is removed;
/// the 1016 bit lives in `PanePixelMouse`.)
pub struct MouseProtocol {
    pub mode: MouseProtocolMode,
    pub encoding: MouseEncoding,
}

/// What one pane offers pixel mouse: whether its child set mode 1016 and
/// the extent the child was told.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PanePixelMouse {
    requested: bool,
    extent: Option<PanePixelExtent>,
}

impl PanePixelMouse {
    pub const OFF: Self;
    pub fn new(requested: bool, extent: Option<PanePixelExtent>) -> Self;
    pub fn requested(self) -> bool;
    pub fn extent(self) -> Option<PanePixelExtent>;
}

/// A pixel position a client mapped into a pane's extent, with the extent
/// it mapped against (so a report that crossed a resize is recognised).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PixelReport {
    x: u32,
    y: u32,
    extent: PanePixelExtent,
}

impl PixelReport {
    pub fn new(x: u32, y: u32, extent: PanePixelExtent) -> Self;
    pub fn x(self) -> u32;
    pub fn y(self) -> u32;
    pub fn extent(self) -> PanePixelExtent;
}

/// The one pixel mouse eligibility rule. A connection whose host measured
/// its cell exactly, looking at a pane whose child asked for 1016 and has a
/// known extent, presented at that extent's grid, may address the pane in
/// pixels; the returned extent is the one to map into. The capture stream,
/// the client's hit mapping and the server's admission all call this.
pub fn pixel_mouse_eligible(
    host: HostCell,
    pane: PanePixelMouse,
    presented: GridSize,
) -> Option<PanePixelExtent>;

/// Admission of one received pixel report: eligible at the report's own
/// grid, mapped against the pane's current extent, and inside it.
pub fn admit_pixel_report(host: HostCell, pane: PanePixelMouse, report: PixelReport) -> bool;

/// Encodes a pointer event for a pane's child, reading the 1016 bit and the
/// extent from the same `PanePixelMouse`:
/// - 1016 and `Pixels`: SGR-pixels at `(x, y)`.
/// - 1016 and `Cell` with an extent: SGR-pixels at `extent.cell_origin`.
/// - 1016 and `Cell` without an extent: SGR at the cell.
/// - no 1016: the cell (a `Pixels` position reports its `column`, `row`),
///   in the child's cell encoding.
pub fn encode_pane_mouse_report(
    kind: MouseEventKind,
    position: Position,
    modifiers: KeyModifiers,
    protocol: MouseProtocol,
    pane: PanePixelMouse,
) -> Option<Vec<u8>>;

/// The host mouse reporting a client is asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HostMouseCapture {
    Off,
    Cells,
    Pixels,
}

impl HostMouseCapture {
    /// `Off` unless `capture`; `Pixels` when also `pixels`.
    pub fn new(capture: bool, pixels: bool) -> Self;
    pub fn enabled(self) -> bool;
    pub fn pixels(self) -> bool;
    /// The client's race guard: a `Pixels` request is applied as `Cells`
    /// when the client's own newest host cell is not exact (the server's
    /// answer to an older geometry).
    pub fn effective(self, host: HostCell) -> Self;
}
```

`encode_mouse_event` stays as the low-level byte encoder.
`encode_pane_mouse_report` is the only caller that picks coordinates.

### 4.3 shepr-vt

- `InputModes`: `sgr_pixel_mouse: bool` becomes `pixel_mouse: PanePixelMouse`;
  `sgr_pixel_mouse_enabled()` becomes `pixel_mouse()`. `mouse_protocol`
  carries no 1016 copy.
- `Terminal::pixel_mouse(&self) -> PanePixelMouse`:
  `PanePixelMouse::new(self.modes.sgr_pixels_mouse, self.current_geometry().pixel_extent())`.
  `input_modes()` builds its field from it. This is the one reader of the
  1016 bit for everything outside the parser.
- `Terminal::text_area_px`, `width_px`, `height_px` are deleted.
- `handler.rs`: `in_band_size_report` and `text_area_pixels_report` read
  `geometry.pixel_extent()`.
- `CSI 16 t` (`ScanEvent::CellSizeQuery`) answers
  `current_geometry().pixel_extent()`'s `cell_pitch()`, so the child's cell
  size times its grid matches the extent it was told even when the extent
  was clamped. For any extent under the clamp the reply is unchanged.

### 4.4 shepr-pty

`fd.rs` sets the winsize from `geometry.pixel_extent()`, writing `0` for an
absent extent with a comment that zero is the winsize ABI's unknown.

### 4.5 shepr-mux

- `PanePixelSize` and `PaneRuntime::pixel_size()` are deleted, along with
  the re-export in `pane.rs`.
- `PaneTerminal::sgr_pixel_mouse_enabled` becomes `pixel_mouse() ->
  PanePixelMouse` (one core lock, `core.terminal.pixel_mouse()`), behind
  `PaneRead::pixel_mouse()`.
- `TerminalDirtyPatchSnapshot.sgr_pixel_mouse` becomes `pixel_mouse:
  PanePixelMouse`, read with `core.terminal.pixel_mouse()` in the same hold
  as the patch.
- `PaneTerminal::encode_mouse_event_with_modes` no longer locks the core:
  `shepr_term::mouse::encode_pane_mouse_report(kind, position, modifiers,
  modes.mouse_protocol()?, modes.pixel_mouse())`. The local `cell_pitch` is
  gone; the pitch is `PanePixelExtent::cell_pitch`.

### 4.6 shepr-protocol

`crates/shepr-protocol/src/geometry.rs`:

```rust
/// A client's host cell as sent; reports stay raw so the server can refuse
/// an oversized one by name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReportedCell {
    Unknown,
    Estimated(CellReport),
    Exact(CellReport),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalGeometry {
    grid: GridSize,
    cell: ReportedCell,
}

impl TerminalGeometry {
    pub fn from_host(grid: GridSize, cell: HostCell) -> Self;
    pub fn grid(self) -> GridSize;
    pub fn surface_size(self) -> ClientSurfaceSize;
    pub fn bounded_grid(self) -> Result<BoundedGridSize, BoundedGridSizeError>;
    /// The server's acceptance of a client geometry: the surface bounds
    /// (`DimensionTooLarge`, `TooManyCells`) and the cell bound
    /// (`CellTooLarge`), then the validated host geometry.
    pub fn host_geometry(self) -> Result<HostGeometry, SurfaceRefusal>;
}
// Deleted: TerminalGeometry::new(cols, rows, width, height, pixel_mouse),
// with_cell, cell, pixel_mouse, cell_geometry, width, height, the
// ReceivedTerminalGeometry shadow and its TryFrom (the enum makes "pixel
// mouse without a cell" unrepresentable), and the ProtocolCellSize alias.
```

`crates/shepr-protocol/src/input.rs`:

```rust
pub enum ClientMousePosition {
    Cell { column: u16, row: u16 },
    Pixels { column: u16, row: u16, report: shepr_term::mouse::PixelReport },
}

pub enum ClientPaneInputEvent {
    // ...
    Mouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        modifiers: WireModifiers,
        lines: u16,
    },
    // ...
}
// Deleted: ClientMouseGeometry and the `geometry` field.
```

`crates/shepr-protocol/src/surface.rs`: `PaneSurfacePane` loses
`sgr_pixel_mouse`, `pixel_width`, `pixel_height` and gains `pixel_mouse:
shepr_term::mouse::PanePixelMouse`.

`crates/shepr-protocol/src/message.rs`: `ServerMessage::MouseCapture { mode:
shepr_term::mouse::HostMouseCapture }` replaces `{ enabled, sgr_pixels }`.

### 4.7 shepr-termio

- `RawInputEvent::HostCellSizeReport { cell: CellReport }`;
  `parse_host_cell_size_report` returns `Option<CellReport>`.
- `HostPixels::pane_position(self, inner: Rect, extent: PanePixelExtent) ->
  Option<(u32, u32)>`. The proportional mapping (`map_axis_within_cell`,
  `boundary`) is unchanged; it reads the extent's width and height.

### 4.8 shepr-surface

`DecodedWireServerMessage::MouseCapture { mode: HostMouseCapture }` in
`crates/shepr-surface/src/decode.rs`; the test fixture pane uses
`pixel_mouse: PanePixelMouse::OFF`.

### 4.9 shepr-server

Connection value (`server/clients.rs`):

```rust
pub(crate) struct ClientConnection {
    pub(crate) shell: ClientShellState,
    pub(crate) terminal_size: GridSize,
    /// The host cell this client last reported: its pixel mode is
    /// `host_cell.is_exact()`.
    pub(crate) host_cell: HostCell,
    // ... (cell_size and pixel_mouse are deleted)
}

pub(crate) struct RenderTarget {
    pub(crate) client_id: ClientId,
    pub(crate) terminal_size: GridSize,
}
impl RenderTarget {
    pub(crate) fn area(&self) -> Rect; // terminal_size.rect()
}
```

`ClientConnection::with_shell(shell, terminal_size, host_cell, activity,
outbox)` and the test constructor `ClientConnection::new((cols, rows),
host_cell, activity, outbox)` take a `HostCell`.

Geometry events (`headless.rs`, B's file; section 9):

- `ShellConnected`: `host_cell = geometry.cell()`. The log line reports
  `cell = ?geometry.cell()` instead of two zero-defaulted integers.
- `ShellResize`: `host_cell = previous.refreshed_by(geometry.cell())`. That
  is exactly today's rule (keep the last known cell, pixel mode follows the
  new report).
- `ClientViewKey { presenting, location_generation, terminal_size, host_cell }`.

Transport (`server/client_transport.rs`): the handshake and
`ClientShellResize` call `geometry.host_geometry()` and refuse or disconnect
with its `SurfaceRefusal`. `client_shell_geometry_error` is deleted.

Admission (`server/pane_input.rs`):

```rust
/// Rewrites every pixel report that may not reach the pane as pixels into
/// its cell position. The decision is `shepr_term::mouse::admit_pixel_report`.
pub(super) fn admit_pixel_reports(
    events: &mut [ClientPaneInputEvent],
    host: HostCell,
    pane: PanePixelMouse,
);
```

`downgrade_ineligible_pixel_mouse` is deleted. `apply_mouse` maps the wire
position to `shepr_term::mouse::Position` with no mode check (`Pixels {
column, row, report }` becomes `Position::Pixels { column, row, x, y }`). The
encoder decides.

`ShellPaneInput` arm (`headless.rs`):
`admit_pixel_reports(&mut events, client.host_cell, runtime.read().pixel_mouse())`.
No outbox read. `ClientOutbox::told_sgr_pixels` is deleted.
`forget_presentation` stays a dedupe reset only.

Outbox (`server/outbox.rs`): `Told.mouse_capture: Option<HostMouseCapture>`;
`tell_mouse_capture(&mut self, mode: HostMouseCapture)`.

Capture stream (`headless/render.rs`, `stream_host_mouse_capture_mode`,
owned here):

```rust
pub(super) fn stream_host_mouse_capture_mode(&mut self) {
    let requested = self.clients.iter().map(|(&client_id, client)| {
        let presenting = self.clients.is_presenting(&client_id);
        let focused = presenting
            .then(|| self.shell_focused_runtime(client_id))
            .flatten();
        // One core hold for both bits.
        let modes = focused.and_then(|(runtime, _)| runtime.read().input_modes());
        let capture = presenting
            && (client.shell_state().mouse_capture
                || modes.is_some_and(|modes| modes.mouse_protocol().is_some()));
        let pixels = presenting
            && focused.zip(modes).is_some_and(|((runtime, _), modes)| {
                self.presented_pane_grid(client_id, runtime).is_some_and(|grid| {
                    pixel_mouse_eligible(client.host_cell, modes.pixel_mouse(), grid).is_some()
                })
            });
        (client_id, HostMouseCapture::new(capture, pixels))
    }).collect::<Vec<_>>();
    // tell_mouse_capture(mode) per client, as today.
}

/// The grid at which `client_id` presents the pane `runtime` of the
/// workspace it views, when that is the PTY's grid: its surface area is the
/// area the workspace's PTYs were last laid out in (layout is a pure
/// function of state and area). `None` when the areas differ: the client
/// shows the pane at another grid, which is never eligible.
fn presented_pane_grid(&self, client_id: ClientId, runtime: &PaneRuntime) -> Option<GridSize>;
```

`presented_pane_grid` reads `viewed_workspace_for_client`, the client's
`terminal_size.rect()` and `AppState::workspace_spawn_geometry(index).area`.
It is a hash lookup and a compare per presenting client, with no layout pass.
It is advisory: it picks the host's report mode. The per-report decisions
(client hit mapping, server admission) check the exact presented grid.

Surface publication (`server/pane_surface.rs`):

- `render_pane_surface(app, target, area)` loses its cell parameter and the
  `inner_rect * cell` product.
- `PaneSurfaceMetadata.sgr_pixel_mouse` becomes `pixel_mouse: PanePixelMouse`,
  from `read.pixel_mouse()` (`from_runtime`) or `snapshot.pixel_mouse`
  (`from_dirty_snapshot`), applied by `apply`. A pane without a runtime gets
  `PanePixelMouse::OFF` (the `Default`).

Render key (`headless/render.rs`, B's file; section 9):
`PaneSurfaceRenderKey = (Option<WorkspaceId>, u16, u16)`;
`SurfaceBoundary::render(&self, app, workspace, area)`; `render_full` uses
`target.area()` and passes no cell.

Spawn and resize geometry (A's and B's files; section 9):

- `SpawnGeometry { area: Rect, cell: Option<CellPx> }`;
  `SpawnGeometry::for_grid(grid, cell: Option<CellPx>)`; `cell_px()` returns
  the field. `or_default` disappears with `HostCellSize`.
  `headless_spawn_geometry` uses `cell: None`.
- `client_views.rs` `client_geometry`: `SpawnGeometry::for_grid(client.terminal_size,
  client.host_cell.cell())`; `apply_workspace_geometry` passes `geometry.cell`.
- `ui::resize_surface(app, resizer, workspace_index, area, cell: Option<CellPx>)`
  and `ui::panes::resize_pane_infos(.., cell: Option<CellPx>)` build
  `PaneGeometry::with_cell(width, height, cell)`.

### 4.10 shepr-client

- `terminal_geometry.rs`: `ioctl_cell_size` returns `Option<CellReport>`;
  `ioctl_host_geometry` builds `HostGeometry::new(grid, HostCell::from_report(report, true))`;
  `current_host_geometry` builds `HostCell::from_report(report, false)` or
  `HostCell::from_host(DEFAULT_CELL_WIDTH_PX, DEFAULT_CELL_HEIGHT_PX, false)`.
  `AtomicCellSize` stores `Option<CellReport>`. `bounded_cell_geometry`
  keeps the cell (`with_grid`).
- `shell_runtime.rs` `view_geometry`: `TerminalGeometry::from_host(grid, host.cell())`.
- `terminal_setup.rs`: `EndpointMouseRequest` is replaced by
  `HostMouseCapture` in `MouseSource::Endpoint`; `desired()` returns a
  `HostMouseCapture` (`Preference` gives `Cells` or `Off`; `Initial` reads the
  atomics). `HostMouseMode::apply(writer, host: HostCell, reassert)`,
  `host_mouse_capture_update(current: HostMouseCapture, requested:
  HostMouseCapture, host: HostCell) -> Option<HostMouseCapture>` and
  `set_mouse_capture_with_writer(writer, mode)` use
  `HostMouseCapture::effective(host)`. `effective_sgr_pixel_mouse` is
  deleted, with its re-export in `lib.rs`. The `capture_active` and
  `sgr_pixels_active` atomics stay (the stdin thread's probe), stored from
  the applied mode. Call sites pass `state.reported_geometry.cell()`.
- `dispatch.rs`: `MouseCapture { mode }` gives `set_mouse_endpoint_request(mode)`.
  `endpoint/message_policy.rs` matches the new shape.
- Shell state (`shell/state.rs`):

  ```rust
  pub(in crate::shell) struct PaneHit {
      // rect, inner_rect, scrollbar_rect, scroll, pane_id, mouse_reporting as today
      pub(in crate::shell) pixel_mouse: PanePixelMouse,
      /// The pane's full content grid when it is shown unclipped, `None`
      /// when the clip cut it (pixels cannot be mapped then).
      pub(in crate::shell) presented: Option<GridSize>,
  }
  ```

  `ClientShellState` gains `host_cell: HostCell` (initially `Unknown`) and
  `set_host_cell(&mut self, cell: HostCell)`. `client_loop.rs`
  `handle_resize` and the state construction in `launch.rs` and `state.rs`
  call it with `reported_geometry.cell()`.
- `shell/input/mouse.rs` `pane_mouse_position`:

  ```rust
  let (column, row) = (mouse.column - inner.x, mouse.row - inner.y); // saturating, as today
  let cell = ClientMousePosition::Cell { column, row };
  let Some(pixels) = self.host_mouse_pixels else { return cell };
  let Some(presented) = hit.presented else { return cell };
  let Some(extent) = pixel_mouse_eligible(self.host_cell, hit.pixel_mouse, presented) else {
      return cell;
  };
  pixels.pane_position(hit.inner_rect, extent).map_or(cell, |(x, y)| {
      ClientMousePosition::Pixels { column, row, report: PixelReport::new(x, y, extent) }
  })
  ```

  `push_pane_mouse_event` and the gesture release in
  `release_input_leases` build `Mouse` events without `geometry`.
- `shell/presentation/surfaces.rs` `pane_geometry_matches` no longer
  compares pixel fields. The extent is runtime metadata like
  `mouse_reporting` and may change in a patch.

### 4.11 Data flow after the change

```
resize (PTY size rule, B) --PaneGeometry--> PaneRuntime::resize
   |-> winsize from pixel_extent()              (child sees E)
   '-> vt Terminal (cols, rows, cell)
          '-> Terminal::pixel_mouse() = (1016 bit, E)
                 |-> InputModes.pixel_mouse -> encoder (cell_origin / pitch from E)
                 |-> PaneRead::pixel_mouse  -> full surface metadata --+
                 '-> dirty snapshot         -> patch metadata --------+-> PaneSurfacePane.pixel_mouse (E, every client)
client: pixel_mouse_eligible(own HostCell, pane E, presented grid) -> PixelReport(x, y, E)
server: capture = pixel_mouse_eligible(conn HostCell, focused pane E, presented grid)
        admit   = admit_pixel_report(conn HostCell, pane E, report)
```

## 5. Migration

Three landings. Each compiles, passes `brokkr check`, and is kept or
reverted as a whole. Within a landing the bricks are ordered so the
implementer can compile crate by crate bottom-up; only the landing boundary
has to be green.

### Landing 1: unknown as a type (behaviour-neutral)

Bricks:

1.1 `shepr-core` geometry: add `CellReport`, make `CellPx` private and
bounded with serde through `CellReport`, delete `within_host_limit`. Replace
`HostCellGeometry` with `HostCell` and reshape `HostGeometry`. Add
`PanePixelExtent`; replace `PaneGeometry::new` and `text_area_px` with
`cells_only` and `pixel_extent`; delete `cell_width` and `cell_height` on
both. Update the module's tests (section 6).

1.2 `shepr-protocol` geometry: `ReportedCell`, the reshaped
`TerminalGeometry`, `host_geometry()`. Delete `ProtocolCellSize`.
`MAX_CELL_SIZE_PX` stays as the alias of `CellPx::MAX_DIMENSION`. Update
`wire_tests.rs` constructors.

1.3 `shepr-term` host: delete `HostCellSize`.

1.4 `shepr-vt`: `handler.rs` and `Terminal::text_area_px` callers move to
`pixel_extent()`. In this landing `width_px` and `height_px` stay as
adapters over `pixel_extent()` so the mux encoder is untouched; they go in
Landing 2. Tests move from `PaneGeometry::new(c, r, 0, 0)` to
`cells_only(c, r)` and from `PaneGeometry::new(c, r, w, h)` to
`with_cell(c, r, CellPx::new(w, h))`. The large-cell test
(`text_area_pixel_report_matches_winsize_limits_for_large_cells`) uses a
cell of `CellPx::MAX_DIMENSION`, still above the clamp at 80 columns.

1.5 `shepr-pty` `fd.rs` winsize from `pixel_extent()`; test constructors as
in 1.4.

1.6 `shepr-mux`: `PaneRuntime::pixel_size()` returns
`Option<PanePixelExtent>` (`current_size.pixel_extent()`), and
`PanePixelSize` is deleted. The `workspace/geometry.rs` test asserts
`pixel_extent()`. Test constructors as in 1.4.

1.7 `shepr-termio`: `HostCellSizeReport { cell: CellReport }`.

1.8 `shepr-server`: `ClientConnection.host_cell` replaces `cell_size` and
`pixel_mouse`. `ClientViewKey`, `RenderTarget` (keeping its cell for this
landing; `RenderTarget::geometry()` returns
`SpawnGeometry::for_grid(terminal_size, host_cell.cell())`),
`SpawnGeometry.cell`, `ui::resize_surface`, `resize_pane_infos` and the
render key carry `Option<CellPx>` where they carried `HostCellSize` (the key
spells the cell as `Option<CellPx>`). `render_pane_surface` keeps
publishing `inner_rect * cell` from `Option<CellPx>`, unclamped, `(0, 0)`
for `None`, exactly as today. `downgrade_ineligible_pixel_mouse` compares
`runtime_pixels: Option<PanePixelExtent>` against the echoed
`ClientMouseGeometry` field by field. The `ShellPaneInput` arm computes
`client.host_cell.is_exact() && client.outbox.told_sgr_pixels()`, the same
rule as today. The capture stream uses `client.host_cell.is_exact()`.
`client_transport.rs` calls `host_geometry()`.

1.9 `shepr-client`: `terminal_geometry.rs`, `shell_runtime.rs`,
`terminal_setup.rs` call sites (`reported_geometry.cell().is_exact()` where
they passed `exact()`; `effective_sgr_pixel_mouse` still exists in this
landing), and test constructors.

1.10 `AGENTS.md`: the `shepr-term` description drops "and cell size" from
"the host's observed theme and cell size". The `shepr-core` description
becomes "shared geometry (grids, host cells, pane pixel extents), layout
and plain types."

Behaviour after Landing 1: identical. The one representational change: a
host cell above `CellPx::MAX_DIMENSION` from an XTWINOPS reply is now a
`CellReport` turned into `HostCell::Estimated(clamped)` by `from_report`,
the same value `HostCellGeometry::from_host` produced.

### Landing 2: one reader of the 1016 bit, one encoder (behaviour-neutral but `CSI 16 t`)

2.1 `shepr-term` mouse: add `PanePixelMouse`, change `Position::Pixels` to
carry its cell, drop `MouseProtocol::pixels_requested`, add
`encode_pane_mouse_report`.

2.2 `shepr-vt`: `InputModes.pixel_mouse`, `Terminal::pixel_mouse()`. Delete
`width_px`, `height_px` and `text_area_px`. `CSI 16 t` answers the
extent's `cell_pitch()`. Tests: `modes_and_kitty_flags_follow_terminal_state`
asserts `input_modes.pixel_mouse().requested()` instead of the two copies;
`pixel_size_reports_need_pixel_geometry_but_character_size_does_not` keeps
its expected bytes (9x18 cells divide evenly).

2.3 `shepr-mux`: `PaneTerminal::pixel_mouse`, `PaneRead::pixel_mouse`,
`TerminalDirtyPatchSnapshot.pixel_mouse`; the encoder delegates to
`encode_pane_mouse_report` without a core lock. `PaneRuntime::pixel_size()`
is deleted.

2.4 `shepr-server`:
- `pane_input.rs` `apply_mouse` drops its mode check. The wire `Pixels`
  maps to `Position::Pixels { column, row, x, y }`.
- `downgrade_ineligible_pixel_mouse` takes `runtime.read().pixel_mouse().extent()`
  for its pixel comparison.
- `pane_surface.rs` metadata carries `PanePixelMouse` and writes
  `pane.sgr_pixel_mouse = pixel_mouse.requested()` into the still-old wire
  field.
- The capture stream reads `input_modes()` once and uses
  `pixel_mouse().requested()`.

Behaviour after Landing 2: identical on every path except `CSI 16 t` on a
pane whose extent was clamped, which now answers the pitch of the clamped
extent.

### Landing 3: one published extent, one eligibility, the wire

3.1 `shepr-term` mouse: `PixelReport`, `pixel_mouse_eligible`,
`admit_pixel_report`, `HostMouseCapture`.

3.2 `shepr-protocol`: `ClientMousePosition::Pixels { column, row, report }`,
`ClientMouseGeometry` and the `geometry` field deleted,
`PaneSurfacePane.pixel_mouse`, `ServerMessage::MouseCapture { mode }`.
Wire tests updated (section 6).

3.3 `shepr-surface` decode and fixture.

3.4 `shepr-termio` `pane_position(inner, extent)`.

3.5 `shepr-server`:
- `pane_surface.rs` publishes `PanePixelMouse` and drops the cell parameter.
- The render key and `SurfaceBoundary::render` lose the cell;
  `RenderTarget` loses `host_cell` and gains `area()`.
- `admit_pixel_reports` replaces `downgrade_ineligible_pixel_mouse`; the
  `ShellPaneInput` arm stops reading the outbox.
- `told_sgr_pixels` is deleted; `Told` and `tell_mouse_capture` take
  `HostMouseCapture`.
- `stream_host_mouse_capture_mode` and `presented_pane_grid` as in 4.9.
- `clients.rs` `track_shell_input` drops `geometry` from its patterns.
- The test double in `headless/tests/surface_delta.rs` drops its
  `_cell_size` parameter.

3.6 `shepr-client`: `PaneHit`, `host_cell` and `set_host_cell`,
`pane_mouse_position`, the gesture release, `terminal_setup.rs` on
`HostMouseCapture`, `dispatch.rs`, `message_policy.rs`,
`pane_geometry_matches`, and test fixtures (`pixel_mouse:
PanePixelMouse::OFF` in `topology.rs`, `composition.rs`,
`surface_patch.rs`, `shell/tests/mod.rs`, `tests/endpoint_choice.rs`).

3.7 `AGENTS.md`: the `shepr-term` description's "child-facing key and mouse
encoding" becomes "child-facing key and mouse encoding, and the pixel mouse
eligibility rule (which connection, pane and report may carry pixel
positions)".

3.8 `notes/`: delete CON-022 and CON-023 from `hunt-consolidations.md` and
TYP-048 from `hunt-types.md`; in TYP-086 delete the
`downgrade_ineligible_pixel_mouse` and `Told.mouse_capture` bullets. The
discrepancy docs keep only current gaps.

Order inside 3.5, for the seen-to-fail gates (section 7): write the tests
first and run them while `render_pane_surface` still publishes the
viewer's product (built as a `PanePixelExtent` from `inner_rect * cell`,
unclamped, so it compiles against the new wire), then switch it to the
runtime's `pixel_mouse()`.

## 6. Test strategy

New and changed tests, by crate. A name is the gate that it exists. Unless
section 7 lists a separate run, `brokkr check` at the landing boundary runs
it.

shepr-core (`geometry.rs` tests):
- `cell_px_refuses_zero_and_oversized_axes` (L1): `CellPx::new(0, 16)`,
  `CellPx::new(MAX + 1, 16)` are `None`; `CellReport::new(MAX + 1, 16)`
  is `Some` and its `cell()` is `None`; decoding an oversized `CellPx` from
  the codec fails.
- `host_cell_from_host_clamps_oversized_reports_to_an_estimate` (L1):
  replaces `host_geometry_keeps_small_grids_and_bounds_cell_exactness`.
- `host_cell_refresh_keeps_the_last_cell_as_an_estimate` (L1): `Exact(c)`
  refreshed by `Unknown` is `Estimated(c)`; refreshed by `Exact(d)` is
  `Exact(d)`; `Unknown` refreshed by `Unknown` is `Unknown`.
- `pane_pixel_extent_clamps_to_winsize_and_derives_one_pitch` (L1):
  replaces `pixel_extent_uses_winsize_limits`. 80x24 at 9x18 gives 720x432
  and pitch 9x18; 80 columns at `MAX_DIMENSION` gives 65535 wide and pitch
  `65535 / 80`; `cells_only` gives `None`.
- `pane_pixel_extent_cell_origin_is_one_based_top_left` (L1).
- `geometry_rejects_zero_components` and
  `pane_geometry_uses_the_shared_minimum_grid` updated to the new
  constructors.

shepr-protocol:
- `terminal_geometry_validation_refuses_oversized_axes_counts_and_cells`
  (L1), moved from the server's
  `client_shell_geometry_rejects_unsafe_dimensions_and_cell_sizes` with the
  same cases.
- `terminal_geometry_round_trips_each_host_cell_kind` (L1): replaces
  `received_geometry_rejects_pixel_mouse_without_cells`,
  `received_oversized_cells_remain_raw_for_server_refusal` (an oversized
  `Exact` report decodes raw and `host_geometry()` refuses it with
  `CellTooLarge`) and `terminal_geometry_uses_its_shared_positional_wire_shape`.
- `clamp_clears_exactness_and_preserves_unknown` moves to shepr-core as
  part of the `HostCell` tests (L1).
- `pixel_mouse_position_carries_its_extent_on_the_wire` (L3): a `Pixels`
  position round-trips with its `PixelReport`; a zero extent axis fails to
  decode.
- The existing `MouseCapture` wire test in `wire_tests.rs` asserts
  `HostMouseCapture::Pixels` (L3).

shepr-term (`mouse.rs` tests):
- `encoder_maps_cells_to_pitch_origins_under_1016_and_pixels_to_their_cell_without_it`
  (L2): the four rules of `encode_pane_mouse_report`.
- `encoder_sends_sgr_cells_when_1016_has_no_extent` (L2).
- `pixel_mouse_needs_an_exact_host_a_requesting_pane_and_a_matching_presentation`
  (L3): each of the four conditions alone fails.
- `admission_refuses_a_stale_extent_and_out_of_range_pixels` (L3): same
  grid other pixels, other grid, `x = 0`, `x = width + 1`, inexact host.
- `host_mouse_capture_downgrades_pixels_on_an_inexact_host` (L3).

shepr-vt (`tests.rs`):
- `cell_size_query_answers_the_published_pitch` (L2): a pane at
  `MAX_DIMENSION` cells and 80 columns answers `CSI 16 t` with
  `65535 / 80` wide, matching `CSI 14 t`'s 65535.
- `modes_and_kitty_flags_follow_terminal_state` updated (L2).
- `input_modes_carry_the_pixel_extent_with_the_1016_bit` (L2): after a
  resize with a cell, `input_modes().pixel_mouse()` equals
  `pixel_mouse()` with the resized extent.

shepr-mux:
- `dirty_snapshot_and_read_report_the_same_pixel_mouse` (L2), in
  `pane/runtime.rs` tests beside the existing snapshot assertion that reads
  `snapshot.sgr_pixel_mouse`.

shepr-termio (`input/mouse.rs`):
- `integer_cell_pitch_ignores_trailing_pixel_remainder` updated to pass
  `PanePixelExtent`s (L3); expected values unchanged.

shepr-server:
- `pane_input.rs`: `ineligible_shell_pixel_mouse_uses_its_canonical_cell_position`,
  `eligible_shell_pixel_mouse_remains_exact` and
  `stale_shell_pixel_geometry_downgrades_to_its_canonical_cell` rewritten on
  `admit_pixel_reports` (L3).
- `pixel_admission_ignores_the_presentation_memo` (L3): a client told
  `Pixels`, then `ShellReplayHostEffects` handled without a new pixel tell
  reaching the memo (`forget_presentation` alone), still gets its pixel
  report to the child as SGR-pixels.
- `surfaces_publish_the_pty_extent_not_the_viewers_cell` (L3): two clients
  of the same surface size with cells 8x16 and 10x20 view one workspace;
  both surfaces carry the runtime's extent, and `PassReport.surface_renders`
  is 1.
- `non_source_client_with_matching_grid_captures_and_delivers_pixels` (L3):
  the same two clients, the pane in 1003, 1006 and 1016. The non-source
  client is told `HostMouseCapture::Pixels`; a `Pixels` report it sends
  with the published extent reaches the PTY as `ESC [ < ... M` in pixels.
- `client_presenting_a_pane_at_another_grid_is_told_cells` (L3): a second
  client with a different surface size viewing the same workspace is told
  `Cells`, and a `Pixels` report from it is admitted as its cell.
- `oversized_pane_publishes_the_clamped_extent_and_admits_pixels` (L3): a
  single exact client with `MAX_DIMENSION` cells. The surface carries the
  clamped extent and a pixel report against it is admitted.
- `client_shell_receives_metadata_then_shell_free_pane_surface` asserts
  `surface.panes[0].pixel_mouse` against the runtime's
  `read().pixel_mouse()` and the metadata-only patch's `pixel_mouse` being
  `requested() == false` (L3).
- `client_shell_mouse_capture_combines_local_preference_with_endpoint_demand`
  asserts `MouseCapture { mode: Off }` then `{ mode: Cells }` (L3).
- `client_pane_pixel_mouse_uses_runtime_pixel_encoding`,
  `client_pane_pixel_mouse_stays_pixel_scaled_when_sgr_is_reasserted` and
  `client_pane_pixel_mouse_falls_back_to_canonical_cell_position` build
  positions with a `PixelReport` (L3); expected bytes unchanged.
- `outbox.rs` memo test reworked to `HostMouseCapture` (L3).
- `a_split_sizes_against_the_recorded_geometry_and_only_then_the_requesters`
  (`app/mod.rs`) asserts `runtime.read().pixel_mouse().extent()` (L2).

shepr-client:
- `pane_pixel_mouse_preserves_pane_relative_pixel_coordinates` builds the
  surface's `pixel_mouse` with an extent over the pane's inner grid
  (39x38 px) and calls `set_host_cell(Exact)` (L3).
- `pane_pixel_mouse_rescales_into_a_foreign_pane_extent` (L3): host cell
  10x20, pane extent from 8x16 cells; the report lands at the matching
  8x16 cell with the in-cell offset scaled.
- `pane_presented_at_another_grid_reports_cells` (L3): extent grid differs
  from the inner rect; the event is `Cell`.
- The clipped-pane case in `chrome_context.rs` (the oversized surface)
  asserts `hit.presented == None` (L3).
- `sgr_pixel_mouse_needs_capture_a_request_and_exact_geometry` becomes
  `host_capture_applies_pixels_only_on_an_exact_host` over
  `HostMouseCapture::effective` (L3).
- `cell_geometry_is_bounded_before_wire_use_and_disables_inexact_pixel_mouse`
  asserts `geometry.cell()` is `Estimated` at `MAX_CELL_SIZE_PX` (L1).
- `a_patch_may_change_a_panes_pixel_extent` (L3, `surfaces.rs` tests): a
  patch whose pane carries a different extent applies.

## 7. Gate commands

Landing 1:

```
brokkr check
```

Landing 2:

```
brokkr check
```

Landing 3. First, with `render_pane_surface` still publishing the viewer's
product (section 5, order inside 3.5), each of these must fail:

```
brokkr test -p shepr-server non_source_client_with_matching_grid_captures_and_delivers_pixels
brokkr test -p shepr-server oversized_pane_publishes_the_clamped_extent_and_admits_pixels
brokkr test -p shepr-server surfaces_publish_the_pty_extent_not_the_viewers_cell
```

Then, with publication switched to the runtime:

```
brokkr check
```

Also seen to fail with its production half reverted:
`pixel_admission_ignores_the_presentation_memo`, run with the
`ShellPaneInput` arm temporarily keeping `client.outbox.told_sgr_pixels()`
as a condition:

```
brokkr test -p shepr-server pixel_admission_ignores_the_presentation_memo
```

## 8. Risks

- Proportional mapping against pitch. The client maps into an extent
  proportionally (`boundary`: `index * extent / count`). The encoder's
  cell-to-pixel uses `cell_pitch` (`extent / cols`). They agree exactly
  whenever the extent is `grid * cell`, which is every unclamped extent.
  They differ by under one pitch only for a clamped extent (above 65535
  pixels on an axis). Accepted; noted so nobody "fixes" one side alone.
- The capture stream's presented grid is an area proxy. A pane whose
  layout rect is below the pane minimum is sized up to the minimum
  (`GridSize::clamped_pane`), so the PTY grid exceeds what the client shows.
  The stream then says `Pixels` while the client's own check sends cells.
  Cost: the host stays in 1016 with no pixel traffic. The per-report checks
  stay exact.
- In-flight reports across a resize are admitted as cells (the extent echo
  differs), as today.
- Held releases (`track_shell_input`) store the admitted position at press
  time. A release synthesised at teardown after a resize carries pixels in
  the old extent, as today.
- Patches now carry extent changes, because the extent becomes part of
  `PaneSurfaceMetadata`, which retained patches refresh
  (`from_dirty_snapshot`). The server applies its own patches through
  `shepr_surface::decode::apply_patch_to_surface`, which replaces a pane's
  wire record whole with no geometry compare. The client adds its own
  stricter `pane_geometry_matches` in `surfaces.rs`. Landing 3 drops the
  pixel fields from that compare; if it kept them, the client would reject
  a patch the server committed, and the two baselines would diverge.
- Estimated source cells. A source client without an exact host tells its
  children a guessed cell (the client's `DEFAULT_CELL_*` or an XTWINOPS
  reply). An exact non-source client then maps into that guessed extent,
  which is what the child believes. This is correct, but the pixels are in
  a geometry that does not match any real screen; unchanged policy.
- Render sharing grows: clients of one area now share one render whatever
  their cells. `render_pane_surface` must not read anything per client;
  after Landing 3 it reads nothing from the target but the area.

## 9. Stopping rule

In scope: the three entries, the 1016 bit's readers, the `CSI 16 t` reply,
and the `HostMouseCapture` wire value that replaces the `(bool, bool)` pair
the capture stream and its memo exchange.

Out of scope (named, not deferred):
- TYP-052 (1-based coordinate newtypes, the framer emitting typed
  positions). `Position::Pixels` gains its cell here, nothing else.
- TYP-087's mirror types: `ClientMousePosition` and `shepr_term::mouse::Position`
  stay two types.
- The PTY size rule (`workspace_geometry_source`) and when geometry is
  applied (B).
- The client's `HostMouseMode` atomics and stdin probe, beyond the mode
  type they are stored from.
- The default guessed cell policy in `terminal_geometry.rs`.
- Key encoding (AGENTS.md's deliberate non-implementation).

## 10. Findings

1. `CSI 16 t` answers the raw cell while winsize, `CSI 14 t` and mode 2048
   report the clamped extent, so a child of a clamped pane sees
   `cell * cols != xpixel`. Fixed in Landing 2.
2. CON-023's clamp bug confirmed. The runtime's `pixel_size()` is clamped,
   the published product is not, so an oversized pane always downgrades
   even for its sole client. Pinned by
   `oversized_pane_publishes_the_clamped_extent_and_admits_pixels`.
3. `ClientMouseGeometry`'s doc says pane extents "are capped by ioctl
   limits", but the server publishes the uncapped product. The doc
   describes the intended design that finding 2 violates.
4. CON-022 understates the mode bit: it is read four ways. The dirty
   snapshot's `mode_get(DecMode::MouseSgrPixels)` is the fourth, and
   `InputModes` holds the bit twice (`sgr_pixel_mouse` and
   `mouse_protocol.pixels_requested`).
5. CON-022 names `ShellReplayHostEffects` for `forget_presentation`, but
   surface activation (`headless/surface_interest.rs`) clears the memo too
   and deliberately does not re-tell until the replay request. Every pixel
   report between a client's activation and its replay is therefore
   downgraded. Pinned by `pixel_admission_ignores_the_presentation_memo`.
6. mux's `Pixels` without 1016 branch (divide by `cell_pitch`) is dead:
   `apply_mouse` already turned such a position into its cell. Landing 2
   keeps one rule (use the carried cell).
7. TYP-086's bullet `downgrade_ineligible_pixel_mouse(.., runtime_pixels:
   Option<(u32, u32)>)` is stale: the parameter is
   `Option<shepr_mux::pane::PanePixelSize>`. Removed in 3.8 with the
   function.
8. `PaneGeometry::cell_width()` and `cell_height()` have no caller outside
   their own module (the users are `HostGeometry`'s same-named methods).
9. `PaneRuntime::current_size()` is an alias of `grid_size()`
   (`crates/shepr-mux/src/pane/runtime.rs`); one test helper uses it. Not
   touched here.
10. Two public types are named `PaneGeometry`:
    `shepr_core::geometry::PaneGeometry` (grid and cell) and
    `shepr_mux::workspace::PaneGeometry` (layout chrome). Not touched here.
11. `HostCellSize::or_default` runs twice per render on the same value
    (`pane_surface_render_key`, then `render_full`), and `SpawnGeometry::for_grid`
    runs it again on values that `from_cell` already validated. Gone with
    the type.
12. The capture stream decides 1016 from the focused pane only. A
    non-focused pane in 1016 gets cell reports mapped to pitch origins.
    That is today's behaviour, kept.
13. `client_transport.rs` comments that `cell_geometry`'s oversize
    fallback "cannot be reached" because `client_shell_geometry_error`
    already refused. Two validators of one value. `host_geometry()`
    replaces both.
14. Patch admission has two rules. `shepr_surface::decode::apply_patch_to_surface`
    is documented as "the same admission rule as encoding and decoding" and
    accepts any metadata for a known pane id. The client's `PaneSurfaces`
    additionally refuses a patch whose pane `rect`, `inner_rect`, `focused`
    or pixel fields differ (`pane_geometry_matches`, refusing with
    `PatchRejection::PaneGeometry`). The server's retained planner is meant
    never to produce such a patch (pane geometry changes go through a full
    render), so the extra rule only guards; this was not verified for
    `focused`. This spec narrows it (section
    8). Folding it into the shared `SurfaceBaseline::admits` would give
    one rule; that is outside this spec.

## 11. Neighbours: assumptions and offered API

### Assumed of B (`notes/spec-app-loop.md`)

- B's spec owns `headless.rs`, `headless/render.rs` (except
  `stream_host_mouse_capture_mode`, owned here) and `client_views.rs`.
  This spec needs these edits in them, and assumes B either leaves the
  code in place or carries the edits into wherever it moves:
  - `ClientViewKey` fields: `host_cell: HostCell` replaces `cell_size` and
    `pixel_mouse` (L1).
  - `ShellConnected` and `ShellResize` arms: the `host_cell` assignments in
    4.9 (L1). `ShellPaneInput`: the admission call in 4.9 with no outbox
    read (L3). `ShellReplayHostEffects` is unchanged.
  - `pane_surface_render_key`, `PaneSurfaceRenderKey`, `SurfaceBoundary::render`
    and `render_full` lose the cell (L3); `render_full` reads
    `target.area()`.
  - `client_geometry` and `apply_workspace_geometry` pass `Option<CellPx>`
    (L1).
- The `stream_host_mouse_capture_mode` trigger stays as today: it runs
  when `host_input_modes_dirty` is taken, which client view key changes,
  topology changes and renders carrying PTY sources set. A change of a
  pane's `PaneGeometry` reaches it through those paths. A cell-only change
  on the source client changes its `ClientViewKey`. A screen-flip resize
  comes with PTY sources. If B's loop rework changes those triggers, it
  must keep "any change to a viewed pane's `PaneGeometry` or to a
  presenting client's `host_cell` re-runs the capture stream before the
  loop sleeps".
- `viewed_workspace_for_client` and `shell_focused_runtime` keep their
  meaning.

### Assumed of A (`notes/spec-data-model.md`)

- `SpawnGeometry` (in `app/state.rs`) keeps its role. This spec changes its
  cell field to `cell: Option<CellPx>`, `for_grid(grid, Option<CellPx>)`,
  and drops `or_default` (L1). If A moves or renames it, the field type
  travels with it.
- `AppState::workspace_spawn_geometry(index)` keeps answering "the area
  this workspace's PTYs were last laid out in" (`record_workspace_geometry`
  after each application of the PTY size rule).
  `presented_pane_grid` depends on that.
- `headless_spawn_geometry` (`app/creation.rs`) produces `cell: None`.

### Offered to both

- `shepr_core::geometry::{CellReport, CellPx, HostCell, HostGeometry,
  PanePixelExtent, PaneGeometry::{with_cell, cells_only, pixel_extent}}`.
- `shepr_term::mouse::{PanePixelMouse, PixelReport, Position,
  HostMouseCapture, pixel_mouse_eligible, admit_pixel_report,
  encode_pane_mouse_report}`.
- `PaneRead::pixel_mouse()` and `TerminalDirtyPatchSnapshot.pixel_mouse`:
  the one runtime read of a pane's pixel mouse state, one core hold.
- `ClientConnection.host_cell`: the one per-connection pixel mode value;
  `host_cell.cell()` is the cell for spawn geometry.
- `render_pane_surface(app, target, area)`: the pane surface depends on
  state and area only, so any surface sharing B designs may key on
  `(workspace, area)`.
- `admit_pixel_reports(events, host_cell, pane_pixel_mouse)` in
  `server/pane_input.rs` for whichever handler B routes pane input through.
