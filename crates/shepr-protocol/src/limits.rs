// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

// Exact source and build-profile fingerprint shared by the wire preamble and
// JSON status API. Compare it through `is_this_build`, never with `==`.
include!(concat!(env!("OUT_DIR"), "/build_identity.rs"));

/// Maximum allowed frame payload size in either direction. Readers
/// reject larger length prefixes to prevent denial-of-service, and
/// `write_message` refuses to produce them, so an oversized message fails at
/// the sender instead of making the peer tear the connection down. The cap
/// carries ordinary surfaces and terminal output while bounding one allocation.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Largest client hello accepted before authentication.
///
/// A hello is only a few hundred bytes; the cap leaves ample room for future
/// fields while keeping the unauthenticated handshake allocation small.
pub(crate) const HANDSHAKE_FRAME_SIZE: usize = 64 * 1024;

/// Bytes used by a direct-terminal message's fixed positional variant index.
///
/// The message enum has few enough variants that every index takes the
/// shortest LEB128 encoding.
const TERMINAL_VARIANT_INDEX_BYTES: usize = 1;

/// Maximum bytes used to encode the ANSI field length at the frame cap.
///
/// The positional codec's unsigned LEB128 encoding of a permitted frame length
/// fits in this reserved prefix.
const TERMINAL_ANSI_LENGTH_PREFIX_BYTES: usize = 3;

/// Positional message overhead subtracted from the general frame budget.
///
/// This combines the variant index and ANSI byte-length prefixes so the
/// direct-terminal field cap is derived from its wire-format overhead.
const MAX_TERMINAL_ENVELOPE_BYTES: usize =
    TERMINAL_VARIANT_INDEX_BYTES + TERMINAL_ANSI_LENGTH_PREFIX_BYTES;

/// Largest direct-terminal ANSI byte field that fits with the current wire
/// envelope at the general frame cap.
pub const MAX_TERMINAL_FRAME_BYTES: usize = MAX_FRAME_SIZE - MAX_TERMINAL_ENVELOPE_BYTES;

/// Largest client-shell endpoint response chunk emitted by the server.
///
/// The chunk leaves room for the message envelope and keeps endpoint
/// output comfortably below the general frame limit.
pub const MAX_ENDPOINT_RESPONSE_CHUNK_BYTES: usize = 512 * 1024;

/// Whether an encoded payload fits in one protocol frame.
pub const fn frame_payload_fits(size: usize) -> bool {
    size <= MAX_FRAME_SIZE
}

/// The one size cap on a single client request, whichever door it comes in
/// by: pane input, a JSON API request line, or a client-shell endpoint command.
///
/// Keeping the request cap below the frame cap leaves room for the positional
/// wire envelope while limiting client-controlled request buffers.
pub const MAX_CLIENT_REQUEST_BYTES: usize = 1024 * 1024;
/// Maximum text payload (bytes) the server accepts in one input message: the
/// data of one `ClientMessage::Input`, or the summed paste, committed text and
/// generated key text of one `ClientShellPaneInput` batch.
///
/// Kept well below `MAX_FRAME_SIZE` so an input message at the limit still fits
/// in one frame with its envelope. The server answers an oversized paste with a
/// rejection notice rather than a disconnect; clients check the same limit
/// before sending so an oversized paste never has to cross the wire.
pub const MAX_INPUT_PAYLOAD: usize = MAX_CLIENT_REQUEST_BYTES;
/// Maximum JSON API request line accepted before parsing.
///
/// Reuses the shared client-request budget so the API line reader and pane
/// input enforce one limit.
pub const MAX_INITIAL_REQUEST_BYTES: usize = MAX_CLIENT_REQUEST_BYTES;
/// Maximum client-shell endpoint command before forwarding to the API.
///
/// Reuses the shared client-request budget so endpoint commands cannot exceed
/// the API request size.
pub const MAX_ENDPOINT_COMMAND_BYTES: usize = MAX_CLIENT_REQUEST_BYTES;

impl crate::ClientPaneInputEvent {
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

/// Encoded bytes budgeted per cell of a full pane surface or terminal redraw.
///
/// The allowance estimates a typical encoded cell with RGB colors, style flags,
/// underline shape, and a hyperlink. More complex styles or long graphemes can
/// exceed it; the render path handles oversized frames.
pub const SURFACE_BYTES_PER_CELL: usize = 16;

/// Largest grid, in cells, a client may request for a pane surface or a
/// direct terminal attach: what one `MAX_FRAME_SIZE` frame carries at
/// `SURFACE_BYTES_PER_CELL`. The server enforces it; a client of the same
/// build can clamp to it before asking.
pub const MAX_SURFACE_CELLS: usize = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL;

/// Largest width or height, in cells, a client may request.
///
/// The per-axis cap prevents very long, narrow grids from bypassing the total
/// cell budget; the cap supports unusually large terminals without unbounded
/// coordinate ranges.
pub const MAX_SURFACE_DIMENSION: u16 = 4096;

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
    if width > MAX_SURFACE_DIMENSION || height > MAX_SURFACE_DIMENSION {
        return None;
    }
    let cells = usize::from(width) * usize::from(height);
    (cells <= MAX_SURFACE_CELLS).then_some(cells)
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
/// The cap accommodates large terminal surfaces and config maps while
/// limiting attacker-controlled collection sizes before allocation.
/// Fields with tighter protocol caps apply those through
/// `serialize_bounded_vec` / `deserialize_bounded_vec` as well.
pub const MAX_COLLECTION_ITEMS: usize = 131_072;

/// Minimum key repeat count charged for a key event.
///
/// A zero repeat count still represents a delivered key event, so byte
/// accounting charges a repetition.
pub(crate) const MIN_KEY_REPEAT_COUNT: u16 = 1;
