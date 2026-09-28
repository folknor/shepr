// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

// Exact source and build-profile fingerprint shared by the wire preamble and
// JSON status API. Compare it through `is_this_build`, never with `==`.
include!(concat!(env!("OUT_DIR"), "/build_identity.rs"));

/// Maximum allowed frame payload size (2 MB) in either direction. Readers
/// reject larger length prefixes to prevent denial-of-service, and
/// `write_message` refuses to produce them, so an oversized message fails at
/// the sender instead of making the peer tear the connection down.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Largest direct-terminal ANSI byte field that fits with the current wire
/// envelope: one byte for the `Terminal` variant index and three for the
/// byte-length varint at the 2 MiB frame cap.
pub const MAX_TERMINAL_FRAME_BYTES: usize = MAX_FRAME_SIZE - 4;

/// Largest client-shell endpoint response chunk emitted by the server. This
/// matches its 512 KiB chunking and leaves room for the message envelope.
pub const MAX_ENDPOINT_RESPONSE_CHUNK_BYTES: usize = 512 * 1024;

/// Whether an encoded payload fits in one protocol frame.
pub const fn frame_payload_fits(size: usize) -> bool {
    size <= MAX_FRAME_SIZE
}

/// Maximum text payload (bytes) the server accepts in one input message: the
/// data of one `ClientMessage::Input`, or the summed paste, committed text and
/// generated key text of one `ClientShellPaneInput` batch.
///
/// Kept well below `MAX_FRAME_SIZE` so an input message at the limit still fits
/// in one frame with its envelope. The server answers an oversized paste with a
/// rejection notice rather than a disconnect; clients check the same limit
/// before sending so an oversized paste never has to cross the wire.
pub const MAX_INPUT_PAYLOAD: usize = 1024 * 1024;

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
                    .saturating_mul(usize::from((*repeat_count).max(1)))
            }),
            Self::TextCommit(text) | Self::Paste(text) => text.len(),
            Self::Mouse { .. } => 0,
        }
    }
}

/// Encoded bytes budgeted per cell of a full pane surface or terminal redraw.
///
/// A typical cell with RGB foreground and background, style flags, underline
/// shape and a hyperlink is about 16 bytes. More complex styles or long
/// graphemes can exceed it; the render path handles oversized frames.
pub const SURFACE_BYTES_PER_CELL: usize = 16;

/// Largest grid, in cells, a client may request for a pane surface or a
/// direct terminal attach: what one `MAX_FRAME_SIZE` frame carries at
/// `SURFACE_BYTES_PER_CELL`. The server enforces it; a client of the same
/// build can clamp to it before asking.
pub const MAX_SURFACE_CELLS: usize = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL;

/// Largest width or height, in cells, a client may request.
pub const MAX_SURFACE_DIMENSION: u16 = 4096;

/// Maximum hyperlinks carried by one pane surface.
pub const MAX_SURFACE_HYPERLINKS: usize = 65_536;

/// Maximum pane and split metadata entries carried by a pane surface.
pub const MAX_SURFACE_PANES: usize = 4096;
pub const MAX_SURFACE_SPLITS: usize = 4096;

/// Maximum path components in a serialized surface split.
pub const MAX_SURFACE_SPLIT_PATH: usize = 4096;

/// Maximum changed spans carried by a patch or delta.
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
pub const MAX_CELL_SIZE_PX: u32 = 4096;
