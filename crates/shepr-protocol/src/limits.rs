// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

// Exact source and build-profile fingerprint shared by the wire preamble and
// JSON status API. Compare it through `is_this_build`, never with `==`.
include!(concat!(env!("OUT_DIR"), "/build_identity.rs"));

/// Maximum payload of one frame in either direction. A message larger than
/// this crosses as several frames (see `framing`), so the cap bounds one read
/// allocation step, not what a message can carry.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Largest message, reassembled from its frames, a client accepts from a
/// server. It bounds the reader's buffer against a corrupt or hostile stream;
/// nothing the server sends is sized to it: a pane surface at
/// `MAX_SURFACE_CELLS` is a small fraction of it, and only a selection copy of
/// an enormous scrollback could reach it (that reply is refused by size).
pub const MAX_MESSAGE_SIZE: usize = 1024 * 1024 * 1024;

/// Largest message a server accepts from a client: one frame. Client messages
/// are input, geometry and endpoint commands, all far smaller; clients encode
/// them with `encode_frame`, which refuses anything larger before sending.
pub const MAX_CLIENT_MESSAGE_SIZE: usize = MAX_FRAME_SIZE;

/// Largest client hello accepted before authentication.
///
/// A hello is only a few hundred bytes; the cap leaves ample room for future
/// fields while keeping the unauthenticated handshake allocation small.
pub(crate) const HANDSHAKE_FRAME_SIZE: usize = 64 * 1024;

/// The one size cap on a single client request, whichever door it comes in
/// by: pane input or a JSON API request line. A client-shell endpoint command
/// is bounded by `MAX_CLIENT_MESSAGE_SIZE`.
///
/// Keeping the request cap below the frame cap leaves room for the positional
/// wire envelope while limiting client-controlled request buffers.
pub const MAX_CLIENT_REQUEST_BYTES: usize = 1024 * 1024;
/// Maximum text payload (bytes) the server accepts in one input message: the
/// summed paste, committed text and generated key text of one
/// `ClientShellPaneInput` batch.
///
/// Kept well below `MAX_CLIENT_MESSAGE_SIZE` so an input message at the limit
/// still fits in one frame with its envelope. The client batcher stays within
/// this total, rejects a single oversized paste locally, and the server sends
/// a rejection notice for an oversized paste received from a peer.
pub const MAX_INPUT_PAYLOAD: usize = MAX_CLIENT_REQUEST_BYTES;

/// Maximum JSON API request line accepted before parsing.
///
/// Reuses the shared client-request budget so the API line reader and pane
/// input enforce one limit.
pub const MAX_INITIAL_REQUEST_BYTES: usize = MAX_CLIENT_REQUEST_BYTES;

/// Maximum expanded pane input events (see
/// [`ClientPaneInputEvent::expanded_event_count`](crate::ClientPaneInputEvent::expanded_event_count))
/// in one `ClientShellPaneInput` message. The client batcher splits messages
/// before they cross it and the server refuses a message past it. The config
/// crate owns the value because it also caps one configured mouse scroll step.
pub const MAX_INPUT_EVENT_BATCH: usize = shepr_config::MAX_INPUT_EVENT_BATCH;

impl crate::ClientPaneInputEvent {
    /// Expanded input work represented by this event, as charged against
    /// `MAX_INPUT_EVENT_BATCH`.
    ///
    /// Key repeats and mouse scroll lines are charged individually; other
    /// events each count once.
    pub fn expanded_event_count(&self) -> usize {
        match self {
            Self::Key { repeat_count, .. } => {
                usize::from((*repeat_count).max(MIN_KEY_REPEAT_COUNT))
            }
            Self::Mouse {
                kind: crate::ClientMouseKind::ScrollUp | crate::ClientMouseKind::ScrollDown,
                lines,
                ..
            } => usize::from((*lines).max(1)),
            Self::TextCommit(_) | Self::Mouse { .. } | Self::Paste(_) => 1,
        }
    }

    /// Text bytes this event delivers to the pane, as charged against
    /// `MAX_INPUT_PAYLOAD`: paste or committed text, or a key's generated text
    /// times its repeat count. Mouse events carry no text.
    pub fn text_bytes(&self) -> usize {
        match self {
            Self::Key {
                repeat_count,
                generated_text,
                ..
            } => generated_text.as_ref().map_or(0, |text| {
                text.len()
                    .saturating_mul(usize::from((*repeat_count).max(MIN_KEY_REPEAT_COUNT)))
            }),
            Self::TextCommit(text) | Self::Paste(text) => text.len(),
            Self::Mouse { .. } => 0,
        }
    }
}

/// Largest grid, in cells, a client may request for a pane surface. Surfaces
/// cross in as many frames as they need, so this is not a frame budget: it
/// bounds the grids the server keeps per pane and per client. It sits far above
/// any real display (an 8K panel of 4-pixel-wide cells is about a million).
/// The server enforces it; a client of the same build can clamp to it before
/// asking.
pub const MAX_SURFACE_CELLS: usize = shepr_config::MAX_TERMINAL_GRID_CELLS;

/// Largest width or height, in cells, a client may request.
///
/// The per-axis cap prevents very long, narrow grids from bypassing the total
/// cell budget; the cap supports unusually large terminals without unbounded
/// coordinate ranges.
pub const MAX_SURFACE_DIMENSION: u16 = shepr_config::MAX_TERMINAL_GRID_DIMENSION;

/// Minimum permitted width or height for a client-requested surface grid.
///
/// A surface used by the terminal renderer must be nonempty on each axis;
/// empty terminal geometry is handled separately by render code.
pub const MIN_SURFACE_DIMENSION: u16 = 1;

/// Maximum hyperlinks carried by one pane surface.
///
/// The cap allows many linked cells while independently bounding the
/// URI table, whose strings can be much larger than their cell references.
pub const MAX_SURFACE_HYPERLINKS: usize = 65_536;

/// Maximum pane metadata entries carried by one pane surface.
///
/// The cap is well above a practical workspace pane count while bounding the
/// metadata vector independently of rendered cells.
pub const MAX_SURFACE_PANES: usize = 4096;

/// Maximum split metadata entries carried by one pane surface.
///
/// The cap bounds layout metadata and matches the pane metadata ceiling.
pub const MAX_SURFACE_SPLITS: usize = 4096;

/// Maximum path components in a serialized surface split.
///
/// The cap bounds traversal work even for a malformed split path; ordinary
/// workspace layouts use only a small number of components.
pub const MAX_SURFACE_SPLIT_PATH: usize = 4096;

/// Maximum changed spans carried by a patch or delta.
///
/// The cap permits fragmented updates while bounding patch bookkeeping.
pub const MAX_SURFACE_PATCH_SPANS: usize = 4096;

/// Returns the checked number of cells in a permitted surface grid.
pub fn surface_grid_size(width: u16, height: u16) -> Option<usize> {
    shepr_config::terminal_grid_cells(width, height)
}

/// Largest reported cell width or height in pixels.
///
/// The cap accepts unusually large display cells while rejecting geometry
/// claims that would make pixel calculations unreasonable.
pub const MAX_CELL_SIZE_PX: u32 = 4096;

/// Smallest divisor used to translate row-major buffer positions to cells.
///
/// Empty or malformed zero-width buffers still need a nonzero row length for
/// position arithmetic, so this is the safe floor.
pub(crate) const MIN_BUFFER_ROW_LEN: usize = 1;

/// Default maximum nesting depth accepted by the positional codec.
///
/// The depth leaves ample room for ordinary config and protocol values while
/// bounding recursive decoder work on hostile input.
pub const DEFAULT_MAX_DEPTH: usize = 128;

/// Maximum number of items in any codec sequence or map.
///
/// The cap is the largest surface's cell grid, the longest sequence on the
/// wire, while limiting attacker-controlled collection sizes before
/// allocation (a claimed length must also fit the remaining input). Fields
/// with tighter protocol caps apply those through `serialize_bounded_vec` /
/// `deserialize_bounded_vec` as well.
pub const MAX_COLLECTION_ITEMS: usize = MAX_SURFACE_CELLS;

/// Minimum key repeat count charged for a key event.
///
/// A zero repeat count still represents a delivered key event, so byte
/// accounting charges a repetition.
pub(crate) const MIN_KEY_REPEAT_COUNT: u16 = 1;
