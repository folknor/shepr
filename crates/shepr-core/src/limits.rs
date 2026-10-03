//! Shared geometry, layout and encoding limits owned by the core crate.

/// Bytes in one binary kibibyte, the unit byte budgets are written in. A
/// defined unit, not a tunable.
pub const KIBIBYTE_BYTES: usize = 1024;

/// Longest UTF-8 encoding of one Unicode scalar value, in bytes: the stack
/// buffer `char::encode_utf8` needs. Fixed by the encoding, not a tunable.
pub const UTF8_MAX_BYTES_PER_CODEPOINT: usize = 4;

/// Entries in the indexed terminal palette: one for every value its `u8`
/// index can address. Fixed by the xterm palette format, not a tunable.
pub const PALETTE_COLOR_COUNT: usize = 1usize << u8::BITS;

/// Maximum expanded input events accepted in one client pane-input message.
///
/// One configured mouse scroll step also fits this budget because every line
/// expands to one pane input event.
pub const MAX_INPUT_EVENT_BATCH: usize = 4096;

/// Maximum width or height in cells for a terminal grid.
///
/// This bounds both configured headless terminals and client-requested pane
/// surfaces without constraining the raw geometry a host terminal can report.
pub const MAX_TERMINAL_GRID_DIMENSION: u16 = 4096;

/// Largest usable host cell axis in pixels. Raw protocol reports may exceed
/// this so the server can refuse them with a specific reason.
pub const MAX_HOST_CELL_PX: u32 = 4096;

/// Maximum number of cells in a terminal grid.
///
/// This bounds both configured headless terminals and client-requested pane
/// surfaces, independently of the per-axis limit.
pub const MAX_TERMINAL_GRID_CELLS: usize = 1 << 22;

/// Total share represented by both children of a normalized split, covering
/// the full layout area.
pub(crate) const SPLIT_RATIO_TOTAL: f32 = 1.0;
/// Smallest permitted first-child share; this keeps an asymmetric split from
/// collapsing either pane.
pub const MIN_SPLIT_RATIO: f32 = 0.1;
/// Largest permitted first-child share, leaving the minimum share to the
/// second pane.
pub const MAX_SPLIT_RATIO: f32 = SPLIT_RATIO_TOTAL - MIN_SPLIT_RATIO;
/// First-child share used when a split has no explicit ratio; this gives both
/// children equal space.
pub const EVEN_SPLIT: f32 = 0.5;

/// Smallest pane grid width in columns; narrower panes leave too little room
/// for terminal text.
pub(crate) const PANE_MIN_COLS: u16 = 4;
/// Smallest pane grid height in rows.
pub(crate) const PANE_MIN_ROWS: u16 = 2;

/// The first ID the allocator hands out.
pub(crate) const FIRST_PANE_ID: u32 = 1;

/// Divider distance in cells accepted when selecting a split for keyboard
/// resize. This absorbs integer-coordinate edge rounding.
pub(crate) const SPLIT_EDGE_MATCH_TOLERANCE_CELLS: u32 = 1;
/// Minimum number of cells assigned to each child when a split has room for
/// both. This keeps each child representable.
pub(crate) const MIN_SPLIT_CHILD_CELLS: u16 = 1;
/// Axis size needed to give both children their minimum extent, derived from
/// each child's minimum.
pub(crate) const MIN_SPLIT_EXTENT_CELLS: u16 = MIN_SPLIT_CHILD_CELLS + MIN_SPLIT_CHILD_CELLS;
/// Fewest panes a workspace keeps when removing or moving panes.
pub(crate) const MIN_WORKSPACE_PANES: usize = 1;
