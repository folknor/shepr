// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

// Protocol identity of this build: a fold of the source fingerprint into
// `1..u32::MAX`, so it changes whenever any source file does.
include!(concat!(env!("OUT_DIR"), "/protocol_identity.rs"));

/// How a server's advertised protocol relates to this build. Client-protocol
/// connections are settled by the preamble; this is for the JSON API, which
/// has none and only learns the server's protocol from its status reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compatibility {
    /// The server reports this build's protocol.
    Compatible,
    /// The server reports another build's protocol.
    DifferentBuild(u32),
    /// The server did not report a protocol.
    Unknown,
}

impl Compatibility {
    pub fn of(server_protocol: Option<u32>) -> Self {
        match server_protocol {
            Some(protocol) if protocol == PROTOCOL_VERSION => Self::Compatible,
            Some(protocol) => Self::DifferentBuild(protocol),
            None => Self::Unknown,
        }
    }

    pub fn is_compatible(self) -> bool {
        self == Self::Compatible
    }

    /// `Some(true)` when compatible, `Some(false)` for another build, `None`
    /// when the server did not say.
    pub fn known(self) -> Option<bool> {
        match self {
            Self::Compatible => Some(true),
            Self::DifferentBuild(_) => Some(false),
            Self::Unknown => None,
        }
    }

    /// `yes`, `no` or `unknown`, for status output.
    pub fn label(self) -> &'static str {
        match self.known() {
            Some(true) => "yes",
            Some(false) => "no",
            None => "unknown",
        }
    }
}

/// Maximum allowed frame payload size (2 MB) in either direction. Readers
/// reject larger length prefixes to prevent denial-of-service, and
/// `write_message` refuses to produce them, so an oversized message fails at
/// the sender instead of making the peer tear the connection down.
pub const MAX_FRAME_SIZE: usize = 2 * 1024 * 1024;

/// Whether an encoded payload fits in one protocol frame.
pub(crate) const fn frame_payload_fits(size: usize) -> bool {
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
pub(crate) fn surface_grid_size(width: u16, height: u16) -> Option<usize> {
    if width > MAX_SURFACE_DIMENSION || height > MAX_SURFACE_DIMENSION {
        return None;
    }
    let cells = usize::from(width) * usize::from(height);
    (cells <= MAX_SURFACE_CELLS).then_some(cells)
}

/// Largest reported cell width or height in pixels.
pub const MAX_CELL_SIZE_PX: u32 = 4096;
