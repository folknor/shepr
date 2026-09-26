//! Wire protocol for shepr server/client communication.
//!
//! Defines the message types, framing, and safety
//! constraints for the binary protocol over local sockets.
//!
//! Client and server are always the same build. Before any codec payload, both
//! ends of a client-protocol connection exchange a fixed raw preamble (magic,
//! `PROTOCOL_VERSION`, `BUILD_ID`; see `protocol::preamble`). It compares the
//! complete build identity before either side decodes messages.

use std::collections::HashMap;
use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize};

use super::codec::{self, CodecError};

// ---------------------------------------------------------------------------
// Protocol constants
// ---------------------------------------------------------------------------

/// Protocol identity of this build: a fold of the source fingerprint into
/// `1..u32::MAX`, so it changes whenever any source file does.
pub const PROTOCOL_VERSION: u32 = crate::build_info::PROTOCOL_VERSION;

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
/// A typical worst cell is a one-byte symbol with RGB foreground and
/// background (4-byte varints each), an underline-styled modifier and a
/// hyperlink index: about 16 bytes. Cells with long grapheme clusters can
/// still exceed it, so a frame at this budget can be oversized; the render
/// path has to cope with that, but ordinary content always fits.
pub const SURFACE_BYTES_PER_CELL: usize = 16;

/// Largest grid, in cells, a client may request for a pane surface or a
/// direct terminal attach: what one `MAX_FRAME_SIZE` frame carries at
/// `SURFACE_BYTES_PER_CELL`. The server enforces it; a client of the same
/// build can clamp to it before asking.
pub const MAX_SURFACE_CELLS: usize = MAX_FRAME_SIZE / SURFACE_BYTES_PER_CELL;

/// Largest width or height, in cells, a client may request.
pub const MAX_SURFACE_DIMENSION: u16 = 4096;

/// Largest reported cell width or height in pixels.
pub const MAX_CELL_SIZE_PX: u32 = 4096;

/// Length of the u32 little-endian length prefix in bytes.
const LENGTH_PREFIX_BYTES: usize = 4;

// ---------------------------------------------------------------------------
// Server-side client render mode
// ---------------------------------------------------------------------------

/// Render pipeline selected by the kind of client handshake.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderEncoding {
    /// Send semantic surfaces for a client-owned shell.
    SemanticFrame,
    /// Send terminal ANSI frames to a direct terminal client.
    TerminalAnsi,
}

// ---------------------------------------------------------------------------
// Client → Server messages
// ---------------------------------------------------------------------------

/// Size of the pane surface requested by a client-owned shell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientSurfaceSize {
    pub cols: u16,
    pub rows: u16,
}

impl ClientSurfaceSize {
    /// Fit a requested grid into one ordinary surface frame. Keep its width
    /// first so the shell layout tracks the host; trim excess height.
    pub fn clamped(self) -> Self {
        let cols = self.cols.clamp(1, MAX_SURFACE_DIMENSION);
        // Bounded by `MAX_SURFACE_DIMENSION` (a u16) via `.min(...)`, so this never truncates.
        #[allow(clippy::cast_possible_truncation)]
        let max_rows =
            (MAX_SURFACE_CELLS / usize::from(cols)).min(usize::from(MAX_SURFACE_DIMENSION)) as u16;
        Self {
            cols,
            rows: self.rows.clamp(1, max_rows),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientKeyKind {
    Press,
    Repeat,
    Release,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientKeyCode {
    Backspace,
    Enter,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    PageUp,
    PageDown,
    Tab,
    BackTab,
    Delete,
    Insert,
    Esc,
    Char(char),
    F(u8),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ClientMouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMouseKind {
    Down(ClientMouseButton),
    Up(ClientMouseButton),
    Drag(ClientMouseButton),
    Moved,
    ScrollUp,
    ScrollDown,
    ScrollLeft,
    ScrollRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMousePosition {
    Cell {
        column: u16,
        row: u16,
    },
    Pixels {
        x: u32,
        y: u32,
        column: u16,
        row: u16,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientMouseGeometry {
    pub cols: u16,
    pub rows: u16,
    pub width_px: u32,
    pub height_px: u32,
}

/// Pane-domain input after the client has classified and consumed shell actions.
///
/// Keys are semantic rather than outer-terminal VT bytes so the target pane can
/// encode them for the child application's negotiated keyboard protocol.
/// A key carries no physical key identity: the Linux host terminal reports
/// none, so a key is identified by its code alone, and a press that committed
/// `generated_text` gets no release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientPaneInputEvent {
    Key {
        code: ClientKeyCode,
        modifiers: u8,
        kind: ClientKeyKind,
        repeat_count: u16,
        shifted_codepoint: Option<u32>,
        generated_text: Option<String>,
    },
    TextCommit(String),
    Mouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        geometry: Option<ClientMouseGeometry>,
        modifiers: u8,
        lines: u16,
    },
    Paste(String),
}

impl ClientKeyKind {
    pub(crate) fn from_crossterm(kind: crossterm::event::KeyEventKind) -> Self {
        match kind {
            crossterm::event::KeyEventKind::Press => Self::Press,
            crossterm::event::KeyEventKind::Repeat => Self::Repeat,
            crossterm::event::KeyEventKind::Release => Self::Release,
        }
    }

    pub(crate) fn to_crossterm(self) -> crossterm::event::KeyEventKind {
        match self {
            Self::Press => crossterm::event::KeyEventKind::Press,
            Self::Repeat => crossterm::event::KeyEventKind::Repeat,
            Self::Release => crossterm::event::KeyEventKind::Release,
        }
    }
}

impl ClientKeyCode {
    pub(crate) fn from_crossterm(code: crossterm::event::KeyCode) -> Option<Self> {
        use crossterm::event::KeyCode;
        Some(match code {
            KeyCode::Backspace => Self::Backspace,
            KeyCode::Enter => Self::Enter,
            KeyCode::Left => Self::Left,
            KeyCode::Right => Self::Right,
            KeyCode::Up => Self::Up,
            KeyCode::Down => Self::Down,
            KeyCode::Home => Self::Home,
            KeyCode::End => Self::End,
            KeyCode::PageUp => Self::PageUp,
            KeyCode::PageDown => Self::PageDown,
            KeyCode::Tab => Self::Tab,
            KeyCode::BackTab => Self::BackTab,
            KeyCode::Delete => Self::Delete,
            KeyCode::Insert => Self::Insert,
            KeyCode::Esc => Self::Esc,
            KeyCode::Char(ch) => Self::Char(ch),
            KeyCode::F(n) => Self::F(n),
            KeyCode::Null => Self::Null,
            _ => return None,
        })
    }

    pub(crate) fn to_crossterm(&self) -> crossterm::event::KeyCode {
        use crossterm::event::KeyCode;
        match self {
            Self::Backspace => KeyCode::Backspace,
            Self::Enter => KeyCode::Enter,
            Self::Left => KeyCode::Left,
            Self::Right => KeyCode::Right,
            Self::Up => KeyCode::Up,
            Self::Down => KeyCode::Down,
            Self::Home => KeyCode::Home,
            Self::End => KeyCode::End,
            Self::PageUp => KeyCode::PageUp,
            Self::PageDown => KeyCode::PageDown,
            Self::Tab => KeyCode::Tab,
            Self::BackTab => KeyCode::BackTab,
            Self::Delete => KeyCode::Delete,
            Self::Insert => KeyCode::Insert,
            Self::Esc => KeyCode::Esc,
            Self::Char(ch) => KeyCode::Char(*ch),
            Self::F(n) => KeyCode::F(*n),
            Self::Null => KeyCode::Null,
        }
    }
}

impl ClientMouseButton {
    pub(crate) fn from_crossterm(button: crossterm::event::MouseButton) -> Self {
        match button {
            crossterm::event::MouseButton::Left => Self::Left,
            crossterm::event::MouseButton::Right => Self::Right,
            crossterm::event::MouseButton::Middle => Self::Middle,
        }
    }

    pub(crate) fn to_crossterm(self) -> crossterm::event::MouseButton {
        match self {
            Self::Left => crossterm::event::MouseButton::Left,
            Self::Right => crossterm::event::MouseButton::Right,
            Self::Middle => crossterm::event::MouseButton::Middle,
        }
    }
}

impl ClientMouseKind {
    pub(crate) fn from_crossterm(kind: crossterm::event::MouseEventKind) -> Option<Self> {
        use crossterm::event::MouseEventKind;
        Some(match kind {
            MouseEventKind::Down(button) => Self::Down(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Up(button) => Self::Up(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Drag(button) => Self::Drag(ClientMouseButton::from_crossterm(button)),
            MouseEventKind::Moved => Self::Moved,
            MouseEventKind::ScrollUp => Self::ScrollUp,
            MouseEventKind::ScrollDown => Self::ScrollDown,
            MouseEventKind::ScrollLeft => Self::ScrollLeft,
            MouseEventKind::ScrollRight => Self::ScrollRight,
        })
    }

    pub(crate) fn to_crossterm(self) -> crossterm::event::MouseEventKind {
        use crossterm::event::MouseEventKind;
        match self {
            Self::Down(button) => MouseEventKind::Down(button.to_crossterm()),
            Self::Up(button) => MouseEventKind::Up(button.to_crossterm()),
            Self::Drag(button) => MouseEventKind::Drag(button.to_crossterm()),
            Self::Moved => MouseEventKind::Moved,
            Self::ScrollUp => MouseEventKind::ScrollUp,
            Self::ScrollDown => MouseEventKind::ScrollDown,
            Self::ScrollLeft => MouseEventKind::ScrollLeft,
            Self::ScrollRight => MouseEventKind::ScrollRight,
        }
    }
}

impl ClientPaneInputEvent {
    /// Text bytes this event delivers to the pane, as charged against
    /// `MAX_INPUT_PAYLOAD`: paste or committed text, or a key's generated text
    /// times its repeat count. Mouse events carry no text.
    pub(crate) fn text_bytes(&self) -> usize {
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

    pub(crate) fn from_terminal_key(key: crate::input::TerminalKey) -> Option<Self> {
        Some(Self::Key {
            code: ClientKeyCode::from_crossterm(key.code)?,
            modifiers: key.modifiers.bits(),
            kind: ClientKeyKind::from_crossterm(key.kind),
            repeat_count: key.repeat_count,
            shifted_codepoint: key.shifted_codepoint,
            generated_text: key.generated_text,
        })
    }

    pub(crate) fn to_raw_input_event(&self) -> crate::raw_input::RawInputEvent {
        match self {
            Self::Key {
                code,
                modifiers,
                kind,
                repeat_count,
                shifted_codepoint,
                generated_text,
            } => {
                let mut key = crate::input::TerminalKey::new(
                    code.to_crossterm(),
                    crossterm::event::KeyModifiers::from_bits_truncate(*modifiers),
                )
                .with_kind(kind.to_crossterm())
                .with_repeat_count(*repeat_count)
                .with_generated_text(generated_text.clone());
                if let Some(shifted_codepoint) = shifted_codepoint {
                    key = key.with_shifted_codepoint(*shifted_codepoint);
                }
                crate::raw_input::RawInputEvent::Key(key)
            }
            // Text commits are handled directly by pane input before this conversion.
            Self::TextCommit(_) => crate::raw_input::RawInputEvent::Unsupported,
            Self::Mouse {
                kind,
                position,
                modifiers,
                ..
            } => {
                let (column, row) = match position {
                    ClientMousePosition::Cell { column, row }
                    | ClientMousePosition::Pixels { column, row, .. } => (*column, *row),
                };
                crate::raw_input::RawInputEvent::Mouse(crossterm::event::MouseEvent {
                    kind: kind.to_crossterm(),
                    column,
                    row,
                    modifiers: crossterm::event::KeyModifiers::from_bits_truncate(*modifiers),
                })
            }
            Self::Paste(text) => crate::raw_input::RawInputEvent::Paste(text.clone()),
        }
    }
}

/// Messages sent from the client to the server over the client protocol socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientMessage {
    /// Direct terminal handshake: selects terminal ANSI frames and announces terminal dimensions.
    TerminalHello {
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
        pixel_mouse: bool,
    },

    /// Raw input bytes read from the client's stdin.
    Input {
        /// Raw terminal input (possibly multi-byte escape sequences).
        #[serde(with = "codec::byte_buf")]
        data: Vec<u8>,
    },

    /// Terminal resize notification from the client.
    Resize {
        /// New terminal width in columns.
        cols: u16,
        /// New terminal height in rows.
        rows: u16,
        /// Width of a terminal cell in physical pixels, or 0 when unavailable.
        cell_width_px: u32,
        /// Height of a terminal cell in physical pixels, or 0 when unavailable.
        cell_height_px: u32,
        /// Whether this resize carries coherent exact geometry for SGR pixel mouse input.
        pixel_mouse: bool,
    },

    /// Graceful disconnect request.
    Detach,

    /// Switch this connection into direct terminal attach mode.
    AttachTerminal {
        /// Terminal id to attach to.
        terminal_id: String,
        /// Replace an existing writable attach owner for this terminal.
        takeover: bool,
    },

    /// Scroll input handled by a direct terminal attach client.
    AttachScroll {
        /// Original input source for routing.
        source: AttachScrollSource,
        /// Scroll direction.
        direction: AttachScrollDirection,
        /// Number of terminal rows to move when using host scrollback.
        lines: u16,
        /// Mouse column relative to the attached terminal, when available.
        column: Option<u16>,
        /// Mouse row relative to the attached terminal, when available.
        row: Option<u16>,
        /// Crossterm-compatible modifier bits for forwarded mouse wheel events.
        modifiers: u8,
    },

    /// Resize the pane viewport of a client-owned shell.
    ClientShellResize {
        cell_width_px: u32,
        cell_height_px: u32,
        surface_size: ClientSurfaceSize,
        /// Whether this resize carries coherent exact geometry for SGR pixel mouse input.
        pixel_mouse: bool,
    },

    /// Deliver client-classified semantic input directly to a stable pane target.
    ClientShellPaneInput {
        pane_id: String,
        events: Vec<ClientPaneInputEvent>,
    },

    /// Invoke one endpoint operation through this client shell's selected connection.
    ClientShellEndpointRequest { boot_id: String, request: String },

    /// Deliver one structured mouse event to a directly attached terminal.
    AttachMouse {
        kind: ClientMouseKind,
        position: ClientMousePosition,
        geometry: Option<ClientMouseGeometry>,
        modifiers: u8,
        lines: u16,
    },

    /// Publish one host terminal color or appearance update observed by a client-owned shell.
    ClientShellHostTheme { update: ClientHostThemeUpdate },

    /// Publish whether the outer terminal containing a client shell has focus.
    ClientShellFocus { focused: bool },

    /// Named JSON control message for client-owned shells.
    EndpointControl { kind: String, data: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHostColor {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl From<crate::terminal_theme::RgbColor> for ClientHostColor {
    fn from(color: crate::terminal_theme::RgbColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

impl From<ClientHostColor> for crate::terminal_theme::RgbColor {
    fn from(color: ClientHostColor) -> Self {
        Self {
            r: color.r,
            g: color.g,
            b: color.b,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostDefaultColorKind {
    Foreground,
    Background,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostAppearance {
    Dark,
    Light,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClientHostThemeUpdate {
    DefaultColor {
        kind: ClientHostDefaultColorKind,
        color: ClientHostColor,
    },
    PaletteColors(Vec<(u8, ClientHostColor)>),
    Appearance(ClientHostAppearance),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachScrollDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttachScrollSource {
    Wheel,
    PageKey {
        /// Original key bytes to forward when the child application owns page keys.
        #[serde(with = "codec::byte_buf")]
        input: Vec<u8>,
    },
}

// ---------------------------------------------------------------------------
// Server → Client messages
// ---------------------------------------------------------------------------

/// A single cell in a rendered frame, serialized independently from ratatui's
/// `Cell` type to keep the wire protocol stable.
// Keep the scalar fields in step with the terminal adapter: `src/pane/terminal.rs`
// constructs `CellData` directly and supplies these values through the packed
// conversion helpers below. Moving the color and underline layout into typed
// wire fields requires migrating that adapter in the same change. The terminal
// core also still exposes underline as a `u8` in `ghostty::CellStyle`, so the
// adapter and core style need to move with the wire fields.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellData {
    /// Grapheme cluster displayed in this cell (usually 1-2 chars).
    pub symbol: String,
    /// Packed color with a high-byte tag: 0 for named colors, 1 for indexed, 2 for RGB.
    pub fg: u32,
    /// Background color as a packed u32.
    pub bg: u32,
    /// Bitmask of style modifiers (bold, italic, etc.) plus Shepr extension bits.
    pub modifier: u16,
    /// Whether this cell should be skipped during diff-based rendering.
    pub skip: bool,
    /// Index into `FrameData::hyperlinks` for this cell's OSC 8 target, if any.
    pub hyperlink: Option<u32>,
}

impl Clone for CellData {
    fn clone(&self) -> Self {
        Self {
            symbol: self.symbol.clone(),
            ..*self
        }
    }

    fn clone_from(&mut self, source: &Self) {
        let mut symbol = std::mem::take(&mut self.symbol);
        symbol.clone_from(&source.symbol);
        *self = Self { symbol, ..*source };
    }
}

impl CellData {
    pub(crate) fn from_ratatui_cell(cell: &ratatui::buffer::Cell) -> Self {
        Self {
            symbol: cell.symbol().to_owned(),
            fg: color_to_u32(cell.fg),
            bg: color_to_u32(cell.bg),
            modifier: modifier_to_u16(cell.modifier),
            skip: cell.diff_option == ratatui::buffer::CellDiffOption::Skip,
            hyperlink: None,
        }
    }
}

/// Cursor shape encoded as a DECSCUSR parameter.
///
/// 0 = terminal default, 1 = blinking block, 2 = steady block,
/// 3 = blinking underline, 4 = steady underline, 5 = blinking bar,
/// 6 = steady bar.
pub type CursorShapeParam = u8;

/// Cursor position within a rendered frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorState {
    /// Column offset (0-based) of the cursor.
    pub x: u16,
    /// Row offset (0-based) of the cursor.
    pub y: u16,
    /// Whether the cursor is visible.
    pub visible: bool,
    /// Cursor shape as a DECSCUSR parameter.
    #[serde(default)]
    pub shape: CursorShapeParam,
}

/// A rendered frame to be displayed by the client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameData {
    /// Cells in row-major order. Length must equal `width * height`.
    pub cells: Vec<CellData>,
    /// Frame width in columns.
    pub width: u16,
    /// Frame height in rows.
    pub height: u16,
    /// Cursor state for this frame, if applicable.
    pub cursor: Option<CursorState>,
    /// OSC 8 hyperlink URIs referenced by cells.
    pub hyperlinks: Vec<String>,
}

impl FrameData {
    /// Creates a `FrameData` from a ratatui `Buffer` and optional cursor.
    ///
    /// This converts ratatui's internal cell representation into the
    /// wire-protocol cell format. The conversion is lossless for all
    /// commonly used cell attributes.
    #[cfg(test)]
    pub fn from_ratatui_buffer(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) -> Self {
        Self::from_ratatui_buffer_with_hyperlinks(buffer, cursor, &[])
    }

    pub fn from_ratatui_buffer_with_hyperlinks(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
        hyperlinks: &[((u16, u16), String, String)],
    ) -> Self {
        let area = buffer.area;
        let width = area.width;
        let height = area.height;

        let mut hyperlink_uris = Vec::<String>::new();
        let mut hyperlink_indices = HashMap::<&str, u32>::new();
        let mut hyperlink_by_position = HashMap::<(u16, u16), (&str, &str)>::new();
        for ((x, y), symbol, uri) in hyperlinks {
            hyperlink_by_position.insert((*x, *y), (symbol.as_str(), uri.as_str()));
        }
        let mut cells = Vec::with_capacity((width as usize) * (height as usize));
        // Walk the buffer's row-major content directly with origin-relative
        // coordinates. `Buffer::cell` takes absolute positions and would miss
        // for a buffer whose area does not start at (0, 0).
        let row_len = usize::from(width).max(1);
        for (position, cell) in buffer.content.iter().enumerate() {
            let (Ok(col), Ok(row)) = (
                u16::try_from(position % row_len),
                u16::try_from(position / row_len),
            ) else {
                break;
            };
            let hyperlink = hyperlink_by_position
                .get(&(col, row))
                .and_then(|(symbol, uri)| {
                    if *symbol != cell.symbol() {
                        return None;
                    }
                    Some(*hyperlink_indices.entry(*uri).or_insert_with(|| {
                        let index = u32::try_from(hyperlink_uris.len()).unwrap_or(u32::MAX);
                        hyperlink_uris.push((*uri).to_owned());
                        index
                    }))
                });
            let mut cell = CellData::from_ratatui_cell(cell);
            cell.hyperlink = hyperlink;
            cells.push(cell);
        }

        FrameData {
            cells,
            width,
            height,
            cursor,
            hyperlinks: hyperlink_uris,
        }
    }

    pub(crate) fn replace_from_ratatui_buffer_preserving_effects(
        &mut self,
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) {
        let width = self.width;
        let hyperlinks = if width == 0 {
            Vec::new()
        } else {
            self.cells
                .iter()
                .enumerate()
                .filter_map(|(index, cell)| {
                    let uri = self.hyperlinks.get(cell.hyperlink? as usize)?;
                    let x = u16::try_from(index % usize::from(width)).ok()?;
                    let y = u16::try_from(index / usize::from(width)).ok()?;
                    Some(((x, y), cell.symbol.clone(), uri.clone()))
                })
                .collect::<Vec<_>>()
        };
        *self = Self::from_ratatui_buffer_with_hyperlinks(buffer, cursor, &hyperlinks);
    }

    /// Reconstructs a ratatui `Buffer` from this frame data.
    ///
    /// Returns `None` if the cells vector length doesn't match `width * height`.
    pub(crate) fn to_ratatui_buffer(&self) -> Option<ratatui::buffer::Buffer> {
        let expected = (self.width as usize) * (self.height as usize);
        if self.cells.len() != expected {
            return None;
        }

        let area = ratatui::layout::Rect::new(0, 0, self.width, self.height);
        let mut buffer = ratatui::buffer::Buffer::filled(area, ratatui::buffer::Cell::new(" "));

        for row in 0..self.height {
            for col in 0..self.width {
                let idx = (row as usize) * (self.width as usize) + (col as usize);
                let cell_data = &self.cells[idx];
                let cell = buffer.cell_mut((col, row))?;
                cell.set_symbol(&cell_data.symbol);
                cell.fg = u32_to_color(cell_data.fg);
                cell.bg = u32_to_color(cell_data.bg);
                cell.modifier = u16_to_modifier(cell_data.modifier);
                cell.set_diff_option(if cell_data.skip {
                    ratatui::buffer::CellDiffOption::Skip
                } else {
                    ratatui::buffer::CellDiffOption::None
                });
            }
        }

        Some(buffer)
    }
}

/// Initial resource projection used by the client-owned shell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellSnapshot {
    /// Changes whenever the endpoint process restarts.
    pub boot_id: String,
    /// Monotonic replacement revision within one endpoint boot.
    pub revision: u64,
    /// Endpoint's normalized built-in keybindings, used only when a remote client selects server bindings.
    pub server_keybindings_toml: Option<String>,
    pub focused_workspace_id: Option<String>,
    pub focused_tab_id: Option<String>,
    pub focused_pane_id: Option<String>,
    pub tab_bar_right: Vec<ClientShellTabStatusSegment>,
    pub tab_bar_right_separator: String,
    pub workspaces: Vec<ClientShellWorkspace>,
    pub tabs: Vec<ClientShellTab>,
    pub panes: Vec<ClientShellPane>,
    pub agents: Vec<ClientShellAgent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTabStatusSegment {
    pub text: String,
    pub accent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellWorkspace {
    pub workspace_id: String,
    pub active_tab_id: String,
    pub new_workspace_cwd: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub branch: Option<String>,
    pub git_ahead_behind: Option<(usize, usize)>,
    pub tokens: Vec<(String, String)>,
    pub focused: bool,
    pub agent_status: crate::api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellTab {
    pub tab_id: String,
    pub workspace_id: String,
    pub number: usize,
    pub label: String,
    pub custom_label: bool,
    pub zoomed: bool,
    pub focused: bool,
    pub agent_status: crate::api::schema::AgentStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellPane {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub label: Option<String>,
    pub cwd: Option<String>,
    pub foreground_cwd: Option<String>,
    pub focused: bool,
    pub right_click_passthrough: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientShellAgent {
    pub pane_id: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub name: Option<String>,
    pub display_agent: Option<String>,
    pub agent: Option<String>,
    pub title: Option<String>,
    pub terminal_title: Option<String>,
    pub terminal_title_stripped: Option<String>,
    pub agent_status: crate::api::schema::AgentStatus,
    pub state_change_seq: u64,
    pub state_labels: Vec<(String, String)>,
    pub tokens: Vec<(String, String)>,
    pub focused: bool,
}

/// Origin-relative geometry for one pane in a rendered pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePane {
    pub pane_id: String,
    pub content_revision: u64,
    pub rect: SurfaceRect,
    pub inner_rect: SurfaceRect,
    pub scrollbar_rect: Option<SurfaceRect>,
    pub scroll: Option<PaneSurfaceScrollMetrics>,
    pub focused: bool,
    pub mouse_reporting: bool,
    pub sgr_pixel_mouse: bool,
    pub alternate_screen_active: bool,
    pub pixel_width: u32,
    pub pixel_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceScrollMetrics {
    pub offset_from_bottom: u64,
    pub max_offset_from_bottom: u64,
    pub viewport_rows: u64,
}

/// One draggable BSP split handle relative to a pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceSplit {
    pub direction: PaneSurfaceSplitDirection,
    pub pos: u16,
    pub area: SurfaceRect,
    pub hit_rect: SurfaceRect,
    pub path: Vec<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PaneSurfaceSplitDirection {
    Horizontal,
    Vertical,
}

/// Wire-safe rectangle relative to a pane surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceRect {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
}

impl From<ratatui::layout::Rect> for SurfaceRect {
    fn from(rect: ratatui::layout::Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

/// One server-rendered active-tab surface without sidebar, tab bar, or overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceFrame {
    /// Endpoint process identity that produced this surface.
    pub boot_id: String,
    /// Projection revision whose focused IDs and topology produced this surface.
    pub projection_revision: u64,
    /// Monotonic revision for full surfaces and incremental patches on one connection.
    pub surface_revision: u64,
    pub frame: FrameData,
    pub panes: Vec<PaneSurfacePane>,
    pub splits: Vec<PaneSurfaceSplit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePatchRow {
    /// Origin-relative surface column where this changed cell span starts.
    pub x: u16,
    /// Origin-relative surface row.
    pub y: u16,
    pub cells: Vec<CellData>,
}

/// The single rule for changed-cell spans against a `width` x `height` grid, shared by
/// every producer and consumer of patch and delta rows: each span is non-empty, lies
/// within one row of the grid, and starts at or after the end of the previous span in
/// row-major order (so spans are sorted and never overlap).
pub(crate) struct PatchSpanCheck {
    width: usize,
    height: u16,
    previous_end: usize,
}

impl PatchSpanCheck {
    pub(crate) fn new(width: u16, height: u16) -> Self {
        Self {
            width: usize::from(width),
            height,
            previous_end: 0,
        }
    }

    /// Accepts the next span or says why it breaks the rule.
    pub(crate) fn push(&mut self, x: u16, y: u16, len: usize) -> Result<(), &'static str> {
        let x = usize::from(x);
        if len == 0 {
            return Err("patch span is empty");
        }
        if y >= self.height || x >= self.width || len > self.width - x {
            return Err("patch span is outside its row");
        }
        let start = usize::from(y)
            .checked_mul(self.width)
            .and_then(|row| row.checked_add(x))
            .ok_or("patch span overflows the grid")?;
        if start < self.previous_end {
            return Err("patch spans overlap or are not sorted");
        }
        self.previous_end = start + len;
        Ok(())
    }
}

/// Checks a whole set of rows with [`PatchSpanCheck`].
pub(crate) fn validate_patch_rows(
    width: u16,
    height: u16,
    rows: &[PaneSurfacePatchRow],
) -> Result<(), &'static str> {
    let mut check = PatchSpanCheck::new(width, height);
    rows.iter()
        .try_for_each(|row| check.push(row.x, row.y, row.cells.len()))
}

/// Puts rows into the row-major order [`PatchSpanCheck`] requires.
pub(crate) fn sort_patch_rows(rows: &mut [PaneSurfacePatchRow]) {
    rows.sort_unstable_by_key(|row| (row.y, row.x));
}

/// Incremental terminal-cell update against one committed complete pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePatch {
    pub boot_id: String,
    pub projection_revision: u64,
    pub base_surface_revision: u64,
    pub surface_revision: u64,
    pub rows: Vec<PaneSurfacePatchRow>,
    /// Updated metadata for panes whose terminal content changed.
    pub panes: Vec<PaneSurfacePane>,
    /// Final cursor relative to the pane surface.
    pub cursor: Option<CursorState>,
}

/// Terminal ANSI bytes encoded by the server for direct terminal-attach clients.
///
/// The client writes `bytes` straight to stdout and needs nothing else, so the
/// frame carries no sequence number, size or full-redraw flag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalFrame {
    /// Terminal escape bytes ready to write directly to stdout.
    #[serde(with = "codec::byte_buf")]
    pub bytes: Vec<u8>,
}

/// Messages sent from the server to the client over the client protocol socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServerMessage {
    /// Direct-terminal handshake response. The hello variant already selects
    /// terminal ANSI frames; errors report why the server rejected the hello.
    Welcome {
        /// If present, the handshake failed and this describes why.
        /// The client should exit with a clear error message.
        error: Option<String>,
    },

    /// Terminal bytes to write directly for a terminal-ANSI client.
    Terminal(TerminalFrame),

    /// Server is shutting down. Clients should exit gracefully.
    ServerShutdown {
        /// Optional reason for the shutdown.
        reason: Option<String>,
    },

    /// OSC 52 clipboard data forwarded from a PTY through the server.
    Clipboard {
        /// Base64-encoded clipboard data.
        data: String,
    },

    /// Set the foreground client's outer terminal window title.
    WindowTitle {
        /// Sanitized title to write with OSC 0. `None` restores Shepr's default title.
        title: Option<String>,
    },

    /// Whether the client should currently capture host mouse input.
    MouseCapture {
        /// True when Shepr mouse UI is enabled or the focused pane app requests mouse reporting.
        enabled: bool,
        /// True only while the focused pane requests DEC SGR pixel mode 1016.
        sgr_pixels: bool,
    },

    /// Active-tab pane content rendered at a client-requested origin-relative size.
    PaneSurface(PaneSurfaceFrame),

    /// Immediate endpoint error that the client-rendered shell must show.
    ClientShellError { message: String },

    /// Exact Kitty keyboard flags requested by a directly attached terminal.
    /// Zero restores the host terminal's previous keyboard mode.
    DirectTerminalKeyboardProtocol {
        flags: u16,
        modify_other_keys_level: u8,
    },

    /// Whether the focused pane needs the shell host to report every key.
    ClientShellKeyboardReportAll { enabled: bool },

    /// One ordered chunk of the final response to an endpoint operation.
    ClientShellEndpointResponseChunk {
        boot_id: String,
        request_id: String,
        final_chunk: bool,
        #[serde(with = "codec::byte_buf")]
        data: Vec<u8>,
    },

    /// Incremental terminal-cell update for a previously committed pane surface.
    PaneSurfacePatch(PaneSurfacePatch),

    /// Named JSON control message for client-owned shells.
    EndpointControl { kind: String, data: String },

    /// Something a direct terminal-attach client must tell its user because
    /// the server could not do what the user asked: input dropped because the
    /// pane stopped reading, a paste over the input limit rejected, or a
    /// screen too large to send in one frame. The text is complete and
    /// human-readable; the client shows it as-is.
    ///
    /// The server rate-limits it: a repeating condition (dropped input,
    /// oversized frames) is sent once until it clears, a rejected paste once
    /// per paste. The connection stays up either way, so the client must not
    /// treat this as fatal and must keep the attached terminal usable.
    DirectTerminalNotice { message: String },
}

// ---------------------------------------------------------------------------
// Color / Modifier conversion helpers
// ---------------------------------------------------------------------------

/// Converts a ratatui `Color` to a packed u32 for wire transport.
///
/// Encoding:
/// - Named colors (Reset, Black, …, White) → `0x00_00_00_XX` where XX is 0..=16
/// - Indexed palette → `0x01_00_00_XX` where XX is the palette index
/// - RGB → `0x02_RR_GG_BB` with components in the lower 3 bytes
pub(crate) fn color_to_u32(color: ratatui::style::Color) -> u32 {
    match color {
        ratatui::style::Color::Reset => 0x00_00_00_00,
        ratatui::style::Color::Black => 0x00_00_00_01,
        ratatui::style::Color::Red => 0x00_00_00_02,
        ratatui::style::Color::Green => 0x00_00_00_03,
        ratatui::style::Color::Yellow => 0x00_00_00_04,
        ratatui::style::Color::Blue => 0x00_00_00_05,
        ratatui::style::Color::Magenta => 0x00_00_00_06,
        ratatui::style::Color::Cyan => 0x00_00_00_07,
        ratatui::style::Color::Gray => 0x00_00_00_08,
        ratatui::style::Color::DarkGray => 0x00_00_00_09,
        ratatui::style::Color::LightRed => 0x00_00_00_0A,
        ratatui::style::Color::LightGreen => 0x00_00_00_0B,
        ratatui::style::Color::LightYellow => 0x00_00_00_0C,
        ratatui::style::Color::LightBlue => 0x00_00_00_0D,
        ratatui::style::Color::LightMagenta => 0x00_00_00_0E,
        ratatui::style::Color::LightCyan => 0x00_00_00_0F,
        ratatui::style::Color::White => 0x00_00_00_10,
        ratatui::style::Color::Indexed(i) => 0x01_00_00_00 | (i as u32),
        ratatui::style::Color::Rgb(r, g, b) => {
            0x02_00_00_00 | ((r as u32) << 16) | ((g as u32) << 8) | (b as u32)
        }
    }
}

/// Converts a packed u32 back to a ratatui `Color`.
fn u32_to_color(val: u32) -> ratatui::style::Color {
    match val >> 24 {
        0x00 => match val & 0xFF {
            0x00 => ratatui::style::Color::Reset,
            0x01 => ratatui::style::Color::Black,
            0x02 => ratatui::style::Color::Red,
            0x03 => ratatui::style::Color::Green,
            0x04 => ratatui::style::Color::Yellow,
            0x05 => ratatui::style::Color::Blue,
            0x06 => ratatui::style::Color::Magenta,
            0x07 => ratatui::style::Color::Cyan,
            0x08 => ratatui::style::Color::Gray,
            0x09 => ratatui::style::Color::DarkGray,
            0x0A => ratatui::style::Color::LightRed,
            0x0B => ratatui::style::Color::LightGreen,
            0x0C => ratatui::style::Color::LightYellow,
            0x0D => ratatui::style::Color::LightBlue,
            0x0E => ratatui::style::Color::LightMagenta,
            0x0F => ratatui::style::Color::LightCyan,
            0x10 => ratatui::style::Color::White,
            _ => ratatui::style::Color::Reset, // unknown named → Reset
        },
        0x01 => ratatui::style::Color::Indexed((val & 0xFF) as u8),
        0x02 => {
            let r = ((val >> 16) & 0xFF) as u8;
            let g = ((val >> 8) & 0xFF) as u8;
            let b = (val & 0xFF) as u8;
            ratatui::style::Color::Rgb(r, g, b)
        }
        _ => ratatui::style::Color::Reset, // unknown tag → Reset
    }
}

const UNDERLINE_STYLE_SHIFT: u16 = 12;
const UNDERLINE_STYLE_MASK: u16 = 0xF000;

/// Converts a ratatui `Modifier` bitmask to a u16 for wire transport.
pub(crate) fn modifier_to_u16(modifier: ratatui::style::Modifier) -> u16 {
    modifier.bits()
}

pub(crate) fn underline_style_from_modifier(modifier: u16) -> u8 {
    ((modifier & UNDERLINE_STYLE_MASK) >> UNDERLINE_STYLE_SHIFT) as u8
}

pub(crate) fn modifier_with_underline_style(
    modifier: ratatui::style::Modifier,
    underline_style: u8,
) -> ratatui::style::Modifier {
    let bits = modifier.bits() | ((u16::from(underline_style) & 0x0F) << UNDERLINE_STYLE_SHIFT);
    ratatui::style::Modifier::from_bits_retain(bits)
}

/// Converts a u16 back to a ratatui `Modifier`, the inverse of
/// `modifier_to_u16`.
///
/// The underline style in bits 12-15 is kept (`from_bits_retain`): the client
/// shell round-trips whole frames through a ratatui buffer whenever it draws a
/// selection, copy-mode highlight, banner, notice or overlay, and truncating
/// here turned every curly, dotted or dashed underline into a plain one for as
/// long as any of those was on screen. The style is dropped only when the
/// cell is not underlined, so a stale style cannot resurface as a styled
/// underline if an overlay later adds a plain `UNDERLINED`.
fn u16_to_modifier(val: u16) -> ratatui::style::Modifier {
    let val = if val & ratatui::style::Modifier::UNDERLINED.bits() == 0 {
        val & !UNDERLINE_STYLE_MASK
    } else {
        val
    };
    ratatui::style::Modifier::from_bits_retain(val)
}

// ---------------------------------------------------------------------------
// Framing: length-prefixed binary messages
// ---------------------------------------------------------------------------

/// Errors that can occur during framing operations.
#[derive(Debug)]
pub enum FramingError {
    /// The decoded payload length exceeds the configured maximum frame size.
    Oversized { claimed: usize, max: usize },
    /// An I/O error occurred while reading or writing.
    Io(io::Error),
    /// Encoding or decoding the payload with the wire codec failed.
    Codec(CodecError),
    /// The connection was closed before a complete frame could be read.
    UnexpectedEof,
}

impl std::fmt::Display for FramingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FramingError::Oversized { claimed, max } => {
                write!(f, "frame size {claimed} exceeds maximum {max}")
            }
            FramingError::Io(e) => write!(f, "I/O error: {e}"),
            FramingError::Codec(e) => write!(f, "codec error: {e}"),
            FramingError::UnexpectedEof => write!(f, "unexpected end of stream"),
        }
    }
}

impl std::error::Error for FramingError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FramingError::Io(e) => Some(e),
            FramingError::Codec(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for FramingError {
    fn from(e: io::Error) -> Self {
        FramingError::Io(e)
    }
}

impl From<CodecError> for FramingError {
    fn from(e: CodecError) -> Self {
        FramingError::Codec(e)
    }
}

/// Serializes a message and writes it as a length-prefixed frame:
/// `[u32LE length][codec payload]` (see `protocol::codec` for the payload format).
///
/// This is a blocking/synchronous write suitable for use with `std::os::unix::net::UnixStream`
/// in blocking mode, or with any `Write` implementor.
///
/// # Errors
///
/// Returns `FramingError::Oversized`, without writing anything, if the payload
/// exceeds `MAX_FRAME_SIZE`. Every reader enforces that cap and drops the
/// connection on a larger frame, so refusing here keeps the failure local to
/// the one message instead of tearing down the peer connection.
pub fn write_message<W: Write, M: Serialize>(writer: &mut W, msg: &M) -> Result<(), FramingError> {
    let frame = encode_frame(msg)?;
    writer.write_all(&frame)?;
    writer.flush()?;
    Ok(())
}

/// Encodes a message as one complete frame, `[u32LE length][codec payload]`,
/// in a single buffer that is returned as is.
///
/// This is the owned-buffer form of [`write_message`] for callers that queue
/// frames rather than write them: the payload is encoded straight behind a
/// placeholder prefix, so no second copy of the frame is ever made. Passing a
/// `Vec` to `write_message` instead would encode into one buffer and then copy
/// all of it into the `Vec`.
///
/// # Errors
///
/// `FramingError::Oversized` if the payload exceeds `MAX_FRAME_SIZE` (the
/// encoded buffer is dropped), or `FramingError::Codec` if encoding fails.
pub fn encode_frame<M: Serialize>(msg: &M) -> Result<Vec<u8>, FramingError> {
    let mut frame = vec![0u8; LENGTH_PREFIX_BYTES];
    let len = codec::encode_into(&mut frame, msg)?;
    if !frame_payload_fits(len) {
        return Err(FramingError::Oversized {
            claimed: len,
            max: MAX_FRAME_SIZE,
        });
    }
    let prefix = u32::try_from(len).map_err(|_| FramingError::Oversized {
        claimed: len,
        max: MAX_FRAME_SIZE,
    })?;
    frame[..LENGTH_PREFIX_BYTES].copy_from_slice(&prefix.to_le_bytes());
    Ok(frame)
}

/// Reads and deserializes a length-prefixed frame from a reader.
///
/// Reassembles partial reads correctly. Rejects frames whose declared
/// length exceeds `max_frame_size` without panicking or allocating
/// oversized buffers.
pub fn read_message<R: Read, M: for<'de> Deserialize<'de>>(
    reader: &mut R,
    max_frame_size: usize,
) -> Result<M, FramingError> {
    // Read the 4-byte length prefix, reassembling partial reads.
    let mut len_buf = [0u8; LENGTH_PREFIX_BYTES];
    read_exact_or_eof(reader, &mut len_buf)?;
    let claimed_len = usize::try_from(u32::from_le_bytes(len_buf)).unwrap_or(usize::MAX);

    if claimed_len > max_frame_size {
        return Err(FramingError::Oversized {
            claimed: claimed_len,
            max: max_frame_size,
        });
    }

    // Read the payload, reassembling partial reads.
    let mut payload = vec![0u8; claimed_len];
    read_exact_or_eof(reader, &mut payload)?;

    // The decoder must consume the full payload. Trailing bytes after the
    // decoded message indicate a protocol violation (e.g., a corrupted length
    // prefix or concatenated payloads) and yield `CodecError::TrailingBytes`.
    codec::from_slice_exact(&payload).map_err(FramingError::Codec)
}

/// Like `Read::read_exact`, but returns `FramingError::UnexpectedEof`
/// when the reader hits end-of-stream before filling the buffer, instead
/// of the generic `io::ErrorKind::UnexpectedEof`.
fn read_exact_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), FramingError> {
    reader.read_exact(buf).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            FramingError::UnexpectedEof
        } else {
            FramingError::Io(e)
        }
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    use super::codec::{self, CodecError};
    use ratatui::style::{Color, Modifier};
    use serde::de::DeserializeOwned;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    #[test]
    fn client_surface_clamp_fits_server_geometry_limit() {
        let surface = ClientSurfaceSize {
            cols: u16::MAX,
            rows: u16::MAX,
        }
        .clamped();
        assert_eq!(surface.cols, MAX_SURFACE_DIMENSION);
        assert!(surface.rows > 0);
        assert!(usize::from(surface.cols) * usize::from(surface.rows) <= MAX_SURFACE_CELLS);
        assert_eq!(
            ClientSurfaceSize { cols: 80, rows: 24 }.clamped(),
            ClientSurfaceSize { cols: 80, rows: 24 }
        );
    }

    /// Encodes and decodes `value` with the wire codec, requiring the decoder
    /// to consume every encoded byte.
    fn roundtrip<T: Serialize + DeserializeOwned>(value: &T) -> Result<T, CodecError> {
        codec::from_slice_exact(&codec::to_vec(value)?)
    }

    // ---- Round-trip: ClientMessage ----

    #[test]
    fn client_hello_roundtrip() -> TestResult {
        let msg = ClientMessage::TerminalHello {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn endpoint_control_roundtrip() -> TestResult {
        let msg = ClientMessage::EndpointControl {
            kind: "endpoint.hello.v1".into(),
            data: r#"{"message":"hello"}"#.into(),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_resize_roundtrip() -> TestResult {
        let msg = ClientMessage::ClientShellResize {
            cell_width_px: 8,
            cell_height_px: 16,
            surface_size: ClientSurfaceSize { cols: 74, rows: 29 },
            pixel_mouse: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_input_roundtrip() -> TestResult {
        let msg = ClientMessage::Input {
            data: vec![0x1b, 0x5b, 0x41], // ESC [ A (up arrow)
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_pane_input_roundtrips_semantic_keys() -> TestResult {
        let message = ClientMessage::ClientShellPaneInput {
            pane_id: "w1:p2".into(),
            events: vec![
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('l'),
                    modifiers: crossterm::event::KeyModifiers::SHIFT.bits(),
                    kind: ClientKeyKind::Release,
                    repeat_count: 1,
                    shifted_codepoint: Some('L' as u32),
                    generated_text: None,
                },
                ClientPaneInputEvent::Key {
                    code: ClientKeyCode::Char('7'),
                    modifiers: crossterm::event::KeyModifiers::CONTROL.bits(),
                    kind: ClientKeyKind::Press,
                    repeat_count: 3,
                    shifted_codepoint: None,
                    generated_text: None,
                },
            ],
        };
        let decoded = roundtrip(&message)?;
        assert_eq!(decoded, message);
        let ClientMessage::ClientShellPaneInput { events, .. } = decoded else {
            panic!("expected targeted semantic input");
        };
        let crate::raw_input::RawInputEvent::Key(semantic) = events[0].to_raw_input_event() else {
            panic!("expected semantic key");
        };
        assert_eq!(semantic.shifted_codepoint, Some('L' as u32));
        assert_eq!(semantic.kind, crossterm::event::KeyEventKind::Release);
        let crate::raw_input::RawInputEvent::Key(key) = events[1].to_raw_input_event() else {
            panic!("expected key");
        };
        assert_eq!(key.code, crossterm::event::KeyCode::Char('7'));
        assert_eq!(key.modifiers, crossterm::event::KeyModifiers::CONTROL);
        assert_eq!(key.repeat_count, 3);
        Ok(())
    }

    #[test]
    fn client_shell_key_roundtrip_keeps_generated_text() {
        let key = crate::input::TerminalKey::new(
            crossterm::event::KeyCode::Char('/'),
            crossterm::event::KeyModifiers::SHIFT,
        )
        .with_generated_text(Some("/".into()));
        let event =
            ClientPaneInputEvent::from_terminal_key(key.clone()).expect("semantic pane key");
        let crate::raw_input::RawInputEvent::Key(roundtripped) = event.to_raw_input_event() else {
            panic!("pane key should remain a key");
        };

        assert_eq!(roundtripped, key);
        assert_eq!(
            crate::input::encode_terminal_key(
                roundtripped,
                crate::input::KeyboardProtocol::Kitty { flags: 1 },
            ),
            b"/"
        );
    }

    #[test]
    fn client_shell_focus_roundtrip() -> TestResult {
        let message = ClientMessage::ClientShellFocus { focused: false };
        assert_eq!(roundtrip(&message)?, message);
        Ok(())
    }

    #[test]
    fn client_shell_host_theme_roundtrip() -> TestResult {
        let message = ClientMessage::ClientShellHostTheme {
            update: ClientHostThemeUpdate::PaletteColors(vec![(
                4,
                ClientHostColor {
                    r: 10,
                    g: 20,
                    b: 30,
                },
            )]),
        };
        assert_eq!(roundtrip(&message)?, message);
        Ok(())
    }

    #[test]
    fn client_shell_endpoint_messages_roundtrip() -> TestResult {
        let request = ClientMessage::ClientShellEndpointRequest {
            boot_id: "boot-a".into(),
            request: r#"{"id":"request-a","method":"session.snapshot","params":{}}"#.into(),
        };
        assert_eq!(roundtrip(&request)?, request);

        let response = ServerMessage::ClientShellEndpointResponseChunk {
            boot_id: "boot-a".into(),
            request_id: "request-a".into(),
            final_chunk: true,
            data: br#"{"id":"request-a","result":{"type":"ok"}}"#.to_vec(),
        };
        assert_eq!(roundtrip(&response)?, response);
        Ok(())
    }

    #[test]
    fn client_input_large_multilingual_payload_roundtrip() -> TestResult {
        let text =
            "你好，今天我们测试一段比较长的语音输入。こんにちは。안녕하세요.\u{1F642}".repeat(1024);
        assert!(text.len() > 64 * 1024);
        assert!(text.len() < MAX_FRAME_SIZE);
        let msg = ClientMessage::Input {
            data: text.as_bytes().to_vec(),
        };

        let encoded = codec::to_vec(&msg)?;
        let (decoded, consumed): (ClientMessage, _) = codec::from_slice(&encoded)?;

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, msg);
        Ok(())
    }

    #[test]
    fn client_resize_roundtrip() -> TestResult {
        let msg = ClientMessage::Resize {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_detach_roundtrip() -> TestResult {
        let msg = ClientMessage::Detach;
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_attach_terminal_roundtrip() -> TestResult {
        let msg = ClientMessage::AttachTerminal {
            terminal_id: "term_123".to_owned(),
            takeover: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_attach_scroll_roundtrip() -> TestResult {
        let msg = ClientMessage::AttachScroll {
            source: AttachScrollSource::Wheel,
            direction: AttachScrollDirection::Up,
            lines: 3,
            column: Some(12),
            row: Some(7),
            modifiers: 4,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    // ---- Round-trip: ServerMessage ----

    #[test]
    fn server_welcome_roundtrip() -> TestResult {
        let msg = ServerMessage::Welcome { error: None };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_welcome_with_error_roundtrip() -> TestResult {
        let msg = ServerMessage::Welcome {
            error: Some("invalid handshake".to_owned()),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_frame_roundtrip_nontrivial() -> TestResult {
        // Build a 3×2 frame with varied styles (≥2×2).
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "H".into(),
                    fg: color_to_u32(Color::Red),
                    bg: color_to_u32(Color::Black),
                    modifier: Modifier::BOLD.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "i".into(),
                    fg: color_to_u32(Color::Green),
                    bg: color_to_u32(Color::Reset),
                    modifier: Modifier::ITALIC.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "!".into(),
                    fg: color_to_u32(Color::Rgb(255, 128, 0)),
                    bg: color_to_u32(Color::Indexed(220)),
                    modifier: (Modifier::BOLD | Modifier::UNDERLINED).bits(),
                    skip: false,
                    hyperlink: Some(0),
                },
                CellData {
                    symbol: " ".into(),
                    fg: color_to_u32(Color::Reset),
                    bg: color_to_u32(Color::Reset),
                    modifier: Modifier::empty().bits(),
                    skip: true,
                    hyperlink: None,
                },
                CellData {
                    symbol: "→".into(), // multi-byte grapheme
                    fg: color_to_u32(Color::Cyan),
                    bg: color_to_u32(Color::Blue),
                    modifier: Modifier::REVERSED.bits(),
                    skip: false,
                    hyperlink: None,
                },
                CellData {
                    symbol: "\u{1F980}".into(), // emoji, wide grapheme cluster
                    fg: color_to_u32(Color::Yellow),
                    bg: color_to_u32(Color::Magenta),
                    modifier: Modifier::empty().bits(),
                    skip: false,
                    hyperlink: None,
                },
            ],
            width: 3,
            height: 2,
            cursor: Some(CursorState {
                x: 0,
                y: 0,
                visible: true,
                shape: 6,
            }),
            hyperlinks: vec!["https://example.com".to_owned()],
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: frame.clone(),
            panes: Vec::new(),
            splits: Vec::new(),
        });
        let decoded = roundtrip(&msg)?;
        assert_eq!(msg, decoded);
        match decoded {
            ServerMessage::PaneSurface(surface) => {
                assert_eq!(surface.frame.cells[2].hyperlink, Some(0));
                assert_eq!(
                    surface.frame.hyperlinks,
                    vec!["https://example.com".to_owned()]
                );
            }
            other => panic!("expected pane surface, got {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn pane_surface_patch_roundtrip() -> TestResult {
        let msg = ServerMessage::PaneSurfacePatch(PaneSurfacePatch {
            boot_id: "boot-1".into(),
            projection_revision: 3,
            base_surface_revision: 7,
            surface_revision: 8,
            rows: vec![PaneSurfacePatchRow {
                x: 2,
                y: 4,
                cells: vec![CellData {
                    symbol: "x".into(),
                    fg: 1,
                    bg: 2,
                    modifier: 3,
                    skip: false,
                    hyperlink: None,
                }],
            }],
            panes: Vec::new(),
            cursor: Some(CursorState {
                x: 2,
                y: 4,
                visible: true,
                shape: 2,
            }),
        });
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_snapshot_roundtrip() {
        let msg = ClientShellSnapshot {
            boot_id: "boot-1".into(),
            revision: 1,
            server_keybindings_toml: Some("[keys]\nprefix = \"ctrl+a\"\n".into()),
            focused_workspace_id: Some("w1".into()),
            focused_tab_id: Some("w1:t1".into()),
            focused_pane_id: Some("w1:p1".into()),
            tab_bar_right: vec![ClientShellTabStatusSegment {
                text: "host".into(),
                accent: false,
            }],
            tab_bar_right_separator: " · ".into(),
            workspaces: vec![ClientShellWorkspace {
                workspace_id: "w1".into(),
                active_tab_id: "w1:t1".into(),
                new_workspace_cwd: "/tmp".into(),
                number: 1,
                label: "shell".into(),
                custom_label: false,
                branch: Some("main".into()),
                git_ahead_behind: None,
                tokens: Vec::new(),
                focused: true,
                agent_status: crate::api::schema::AgentStatus::Idle,
            }],
            tabs: vec![ClientShellTab {
                tab_id: "w1:t1".into(),
                workspace_id: "w1".into(),
                number: 1,
                label: "main".into(),
                custom_label: true,
                zoomed: false,
                focused: true,
                agent_status: crate::api::schema::AgentStatus::Idle,
            }],
            panes: vec![ClientShellPane {
                pane_id: "w1:p1".into(),
                workspace_id: "w1".into(),
                tab_id: "w1:t1".into(),
                label: None,
                cwd: Some("/repo".into()),
                foreground_cwd: Some("/repo".into()),
                focused: true,
                right_click_passthrough: false,
            }],
            agents: Vec::new(),
        };
        let encoded = serde_json::to_string(&msg).expect("test precondition");
        let decoded: ClientShellSnapshot =
            serde_json::from_str(&encoded).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn server_shutdown_roundtrip() -> TestResult {
        let msg = ServerMessage::ServerShutdown {
            reason: Some("updating".to_owned()),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_clipboard_roundtrip() -> TestResult {
        let msg = ServerMessage::Clipboard {
            data: "dGVzdA==".to_owned(), // base64 "test"
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn server_window_title_roundtrip() -> TestResult {
        for title in [Some("shepr api".to_owned()), None] {
            let msg = ServerMessage::WindowTitle { title };
            assert_eq!(roundtrip(&msg)?, msg);
        }
        Ok(())
    }

    #[test]
    fn server_terminal_frame_roundtrip() -> TestResult {
        let msg = ServerMessage::Terminal(TerminalFrame {
            bytes: b"\x1b[1;1Hhello".to_vec(),
        });
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn byte_fields_encode_as_length_then_raw_bytes() -> TestResult {
        // The byte-buffer fields must keep the plain `Vec<u8>` wire layout
        // (varint length, raw bytes) while decoding in one copy.
        let data = vec![0u8, 1, 0x7f, 0x80, 0xff];
        let encoded = codec::to_vec(&ClientMessage::Input { data: data.clone() })?;
        assert_eq!(encoded.first(), Some(&1), "Input is variant 1");
        assert_eq!(encoded.get(1), Some(&5), "length prefix");
        assert_eq!(encoded.get(2..), Some(data.as_slice()));
        assert_eq!(codec::to_vec(&data)?, encoded.get(1..).unwrap_or_default());

        let large = ClientMessage::Input {
            data: (0..=255u8).cycle().take(300_000).collect(),
        };
        assert_eq!(roundtrip(&large)?, large);

        let page_key = ClientMessage::AttachScroll {
            source: AttachScrollSource::PageKey {
                input: b"\x1b[5~".to_vec(),
            },
            direction: AttachScrollDirection::Up,
            lines: 1,
            column: None,
            row: None,
            modifiers: 0,
        };
        assert_eq!(roundtrip(&page_key)?, page_key);

        // JSON keeps accepting the number-array form serde uses for `Vec<u8>`.
        let json = serde_json::to_string(&ClientMessage::Input { data: data.clone() })?;
        let decoded: ClientMessage = serde_json::from_str(&json)?;
        assert_eq!(decoded, ClientMessage::Input { data });
        Ok(())
    }

    #[test]
    fn server_mouse_capture_roundtrip() -> TestResult {
        let msg = ServerMessage::MouseCapture {
            enabled: true,
            sgr_pixels: true,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn client_shell_keyboard_report_all_roundtrip() -> TestResult {
        let msg = ServerMessage::ClientShellKeyboardReportAll { enabled: true };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn direct_terminal_keyboard_mode_roundtrip() -> TestResult {
        let msg = ServerMessage::DirectTerminalKeyboardProtocol {
            flags: 15,
            modify_other_keys_level: 1,
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    #[test]
    fn direct_terminal_notice_roundtrip() -> TestResult {
        let msg = ServerMessage::DirectTerminalNotice {
            message: "Paste rejected: too large".to_owned(),
        };
        assert_eq!(roundtrip(&msg)?, msg);
        Ok(())
    }

    // ---- Framing ----

    #[test]
    fn framing_small_message_roundtrip() {
        let msg = ClientMessage::TerminalHello {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_large_payload_roundtrip() {
        // Create a pane-surface message that is ≥128 KB.
        // Use a large frame with verbose cell data to exceed 128 KB after encoding.
        // 200×50 = 10000 cells. With varied symbols and styles, this should easily exceed 128 KB.
        let width: u16 = 200;
        let height: u16 = 50;
        let cells: Vec<CellData> = (0..(width as usize) * (height as usize))
            .map(|i| CellData {
                symbol: if i % 256 < 32 {
                    " ".to_owned()
                } else {
                    format!("{:03}", i % 1000)
                },
                fg: color_to_u32(Color::Rgb(
                    u8::try_from(i % 256).unwrap_or(u8::MAX),
                    u8::try_from((i / 256) % 256).unwrap_or(u8::MAX),
                    128,
                )),
                bg: color_to_u32(Color::Indexed(u8::try_from(i % 256).unwrap_or(u8::MAX))),
                modifier: u16::try_from(i % 16).unwrap_or(u16::MAX),
                skip: i % 100 == 0,
                hyperlink: None,
            })
            .collect();

        let frame = FrameData {
            cells,
            width,
            height,
            cursor: Some(CursorState {
                x: 10,
                y: 5,
                visible: true,
                shape: 0,
            }),
            hyperlinks: Vec::new(),
        };
        let msg = ServerMessage::PaneSurface(PaneSurfaceFrame {
            boot_id: "boot-1".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame,
            panes: Vec::new(),
            splits: Vec::new(),
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        // Verify the payload is at least 128 KB
        assert!(
            buf.len() >= 128 * 1024,
            "framed payload should be >= 128 KB, got {} bytes",
            buf.len()
        );

        let decoded: ServerMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_multiple_messages_sequential() {
        // Write 100+ messages of varying types and read them back.
        let mut buf = Vec::new();
        let mut expected = Vec::new();

        for i in 0..150u32 {
            let msg = match i % 5 {
                0 => ClientMessage::TerminalHello {
                    cols: (80 + u16::try_from(i % 40).unwrap_or(u16::MAX)),
                    rows: (24 + u16::try_from(i % 20).unwrap_or(u16::MAX)),
                    cell_width_px: 8,
                    cell_height_px: 16,
                    pixel_mouse: i % 2 == 0,
                },
                1 => ClientMessage::Input {
                    data: vec![u8::try_from(i % 256).unwrap_or(u8::MAX); (i as usize % 50) + 1],
                },
                2 => ClientMessage::ClientShellFocus {
                    focused: i % 2 == 0,
                },
                3 => ClientMessage::Resize {
                    cols: (100 + u16::try_from(i % 30).unwrap_or(u16::MAX)),
                    rows: (30 + u16::try_from(i % 10).unwrap_or(u16::MAX)),
                    cell_width_px: 8,
                    cell_height_px: 16,
                    pixel_mouse: i % 2 == 0,
                },
                4 => ClientMessage::Detach,
                _ => unreachable!(),
            };
            write_message(&mut buf, &msg).expect("test precondition");
            expected.push(msg);
        }

        let mut cursor = buf.as_slice();
        for expected_msg in &expected {
            let decoded: ClientMessage =
                read_message(&mut cursor, MAX_FRAME_SIZE).expect("test precondition");
            assert_eq!(*expected_msg, decoded);
        }
    }

    #[test]
    fn framing_oversized_rejected_without_panic() {
        // Craft a frame with a huge length prefix (4 GB claim).
        let mut buf: Vec<u8> = (u32::MAX).to_le_bytes().to_vec();
        // Add a few garbage bytes after the length prefix.
        buf.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::Oversized { claimed, max }) => {
                assert_eq!(claimed, u32::MAX as usize);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("expected Oversized error, got: {other:?}"),
        }
    }

    #[test]
    fn framing_malformed_payload_rejected_without_panic() {
        // Valid length prefix pointing to garbage data.
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x02];
        let mut buf = u32::try_from(payload.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes()
            .to_vec();
        buf.extend_from_slice(&payload);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err(), "malformed payload should be rejected");
        match result {
            Err(FramingError::Codec(_)) => {} // expected
            other => panic!("expected codec error, got: {other:?}"),
        }
    }

    #[test]
    fn framing_truncated_stream_returns_unexpected_eof() {
        // Write a length prefix claiming 100 bytes, but only provide 4.
        let mut buf: Vec<u8> = 100u32.to_le_bytes().to_vec();
        buf.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::UnexpectedEof) => {}
            other => panic!("expected UnexpectedEof, got: {other:?}"),
        }
    }

    #[test]
    fn framing_zero_length_message() {
        // The smallest real message: Detach encodes as its one-byte variant index.
        let msg = ClientMessage::Detach;
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");

        // Verify the length prefix is correct
        let len = u32::from_le_bytes(buf[..4].try_into().expect("test precondition")) as usize;
        assert_eq!(
            len,
            buf.len() - 4,
            "length prefix should match payload size"
        );

        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn framing_partial_read_reassembly() {
        // Simulate partial reads by using a reader that yields small chunks.
        let msg = ClientMessage::Input {
            data: vec![42; 500], // 500-byte input payload
        };
        let mut full_buf = Vec::new();
        write_message(&mut full_buf, &msg).expect("test precondition");

        // Wrap in a chunked reader that only yields 7 bytes at a time.
        let mut chunked = ChunkedReader::new(full_buf, 7);
        let decoded: ClientMessage =
            read_message(&mut chunked, MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    // ---- Malformed/oversized input ----

    #[test]
    fn oversized_frame_does_not_panic() {
        // Claim 4GB payload - should return Oversized error, not panic.
        let mut buf: Vec<u8> = 0xFFC00000u32.to_le_bytes().to_vec(); // ~4 GB claim
        buf.extend_from_slice(&[0; 8]);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err());
        // Did not panic - test passing is proof.
    }

    #[test]
    fn malformed_frame_does_not_panic() {
        // Random garbage bytes after a valid-ish length prefix.
        let garbage: Vec<u8> = (0..200i32)
            .map(|i| u8::try_from(i ^ 0xAA).unwrap_or(u8::MAX))
            .collect();
        let mut buf = u32::try_from(garbage.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes()
            .to_vec();
        buf.extend_from_slice(&garbage);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        assert!(result.is_err());
        // Did not panic.
    }

    #[test]
    fn oversized_input_rejected_custom_max() {
        // Verify a custom (small) max_frame_size is enforced.
        let msg = ClientMessage::Input {
            data: vec![0x41; 1000],
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");

        let result: Result<ClientMessage, FramingError> = read_message(&mut buf.as_slice(), 64);
        // The encoded payload for 1000 bytes of input will be > 64 bytes.
        assert!(
            matches!(result, Err(FramingError::Oversized { .. })),
            "expected Oversized with small max_frame_size"
        );
    }

    // ---- FrameData ↔ ratatui Buffer conversion ----

    #[test]
    fn frame_data_roundtrip_through_ratatui_buffer() {
        let area = ratatui::layout::Rect::new(0, 0, 5, 3);
        let mut buffer = ratatui::buffer::Buffer::filled(area, ratatui::buffer::Cell::new(" "));

        // Write some styled content.
        buffer
            .cell_mut((0, 0))
            .expect("test precondition")
            .set_symbol("H");
        buffer.cell_mut((0, 0)).expect("test precondition").fg = Color::Red;
        buffer.cell_mut((0, 0)).expect("test precondition").modifier = Modifier::BOLD;

        buffer
            .cell_mut((1, 0))
            .expect("test precondition")
            .set_symbol("i");
        buffer.cell_mut((1, 0)).expect("test precondition").fg = Color::Green;
        buffer.cell_mut((1, 0)).expect("test precondition").modifier = Modifier::ITALIC;

        buffer
            .cell_mut((2, 0))
            .expect("test precondition")
            .set_symbol("!");
        buffer.cell_mut((2, 0)).expect("test precondition").fg = Color::Rgb(255, 128, 0);
        buffer.cell_mut((2, 0)).expect("test precondition").bg = Color::Indexed(220);

        let cursor = CursorState {
            x: 1,
            y: 0,
            visible: true,
            shape: 0,
        };
        let frame = FrameData::from_ratatui_buffer(&buffer, Some(cursor.clone()));

        // Verify frame dimensions.
        assert_eq!(frame.width, 5);
        assert_eq!(frame.height, 3);
        assert_eq!(frame.cells.len(), 15);
        assert_eq!(frame.cursor, Some(cursor));

        // Verify specific cells survived the conversion.
        assert_eq!(frame.cells[0].symbol, "H");
        assert_eq!(frame.cells[0].fg, color_to_u32(Color::Red));
        assert_eq!(frame.cells[0].modifier, Modifier::BOLD.bits());

        assert_eq!(frame.cells[1].symbol, "i");
        assert_eq!(frame.cells[1].fg, color_to_u32(Color::Green));
        assert_eq!(frame.cells[1].modifier, Modifier::ITALIC.bits());

        assert_eq!(frame.cells[2].symbol, "!");
        assert_eq!(frame.cells[2].fg, color_to_u32(Color::Rgb(255, 128, 0)));
        assert_eq!(frame.cells[2].bg, color_to_u32(Color::Indexed(220)));

        let with_links = FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            None,
            &[((1, 0), "i".to_owned(), "https://example.com".to_owned())],
        );
        assert_eq!(with_links.cells[1].hyperlink, Some(0));
        assert_eq!(
            with_links.hyperlinks,
            vec!["https://example.com".to_owned()]
        );

        // Convert back to ratatui buffer and compare.
        let restored = frame.to_ratatui_buffer().expect("should reconstruct");
        assert_eq!(restored.area, area);
        assert_eq!(
            restored.cell((0, 0)).expect("test precondition").symbol(),
            "H"
        );
        assert_eq!(
            restored.cell((0, 0)).expect("test precondition").fg,
            Color::Red
        );
        assert_eq!(
            restored.cell((0, 0)).expect("test precondition").modifier,
            Modifier::BOLD
        );
        assert_eq!(
            restored.cell((1, 0)).expect("test precondition").symbol(),
            "i"
        );
        assert_eq!(
            restored.cell((2, 0)).expect("test precondition").symbol(),
            "!"
        );
        assert_eq!(
            restored.cell((2, 0)).expect("test precondition").fg,
            Color::Rgb(255, 128, 0)
        );
    }

    #[test]
    fn frame_data_rejects_mismatched_cell_count() {
        let frame = FrameData {
            cells: vec![
                CellData {
                    symbol: "X".into(),
                    fg: 0,
                    bg: 0,
                    modifier: 0,
                    skip: false,
                    hyperlink: None,
                };
                5
            ], // 5 cells but 3×2 = 6 expected
            width: 3,
            height: 2,
            cursor: None,
            hyperlinks: Vec::new(),
        };
        assert!(frame.to_ratatui_buffer().is_none());
    }

    // ---- Color conversion coverage ----

    #[test]
    fn color_roundtrip_all_named_colors() {
        let named = [
            Color::Reset,
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::Gray,
            Color::DarkGray,
            Color::LightRed,
            Color::LightGreen,
            Color::LightYellow,
            Color::LightBlue,
            Color::LightMagenta,
            Color::LightCyan,
            Color::White,
        ];
        for c in named {
            assert_eq!(
                u32_to_color(color_to_u32(c)),
                c,
                "roundtrip failed for {c:?}"
            );
        }
    }

    #[test]
    fn color_roundtrip_indexed() {
        for i in 0..=255u8 {
            let c = Color::Indexed(i);
            assert_eq!(
                u32_to_color(color_to_u32(c)),
                c,
                "roundtrip failed for Indexed({i})"
            );
        }
    }

    #[test]
    fn color_roundtrip_rgb() {
        let c = Color::Rgb(0xAB, 0xCD, 0xEF);
        assert_eq!(u32_to_color(color_to_u32(c)), c);

        let c = Color::Rgb(0, 0, 0);
        assert_eq!(u32_to_color(color_to_u32(c)), c);

        let c = Color::Rgb(255, 255, 255);
        assert_eq!(u32_to_color(color_to_u32(c)), c);
    }

    // ---- Modifier conversion ----

    #[test]
    fn modifier_roundtrip() {
        let all_mods = [
            Modifier::BOLD,
            Modifier::ITALIC,
            Modifier::REVERSED,
            Modifier::UNDERLINED,
            Modifier::DIM,
            Modifier::SLOW_BLINK,
            Modifier::CROSSED_OUT,
            Modifier::BOLD | Modifier::ITALIC,
            Modifier::BOLD | Modifier::UNDERLINED | Modifier::REVERSED,
            Modifier::empty(),
        ];
        for m in all_mods {
            assert_eq!(
                u16_to_modifier(modifier_to_u16(m)),
                m,
                "roundtrip failed for {m:?}"
            );
        }
    }

    #[test]
    fn underline_style_survives_ratatui_buffer_roundtrip() {
        // Curly (3), dotted (4) and dashed (5) underlines ride in the
        // modifier's high bits and must survive the client's composition
        // round trip through a ratatui buffer.
        for style in [2u8, 3, 4, 5] {
            let modifier = modifier_to_u16(modifier_with_underline_style(
                Modifier::UNDERLINED | Modifier::BOLD,
                style,
            ));
            let frame = FrameData {
                cells: vec![CellData {
                    symbol: "u".into(),
                    fg: 0,
                    bg: 0,
                    modifier,
                    skip: false,
                    hyperlink: None,
                }],
                width: 1,
                height: 1,
                cursor: None,
                hyperlinks: Vec::new(),
            };
            let buffer = frame.to_ratatui_buffer().expect("test precondition");
            let mut restored = frame.clone();
            restored.replace_from_ratatui_buffer_preserving_effects(&buffer, None);
            assert_eq!(restored.cells[0].modifier, modifier, "style {style}");
            assert_eq!(
                underline_style_from_modifier(restored.cells[0].modifier),
                style
            );
        }
    }

    #[test]
    fn underline_style_is_dropped_from_cells_that_are_not_underlined() {
        let stale = modifier_to_u16(modifier_with_underline_style(Modifier::BOLD, 3));
        assert_eq!(u16_to_modifier(stale), Modifier::BOLD);
    }

    #[test]
    fn read_message_rejects_trailing_bytes() -> TestResult {
        // Encode a valid message, then append an extra byte after it.
        let msg = ClientMessage::Detach;
        let mut payload = codec::to_vec(&msg)?;
        let original_len = payload.len();
        payload.push(0xDE); // trailing garbage

        // Frame it with the inflated length (original + 1).
        let mut buf = u32::try_from(payload.len())?.to_le_bytes().to_vec();
        buf.extend_from_slice(&payload);

        let result: Result<ClientMessage, FramingError> =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE);
        match result {
            Err(FramingError::Codec(error)) => {
                assert_eq!(
                    error,
                    CodecError::TrailingBytes {
                        consumed: original_len,
                        total: original_len + 1,
                    }
                );
                let message = error.to_string();
                assert!(
                    message.contains("trailing bytes"),
                    "error should mention trailing bytes: {message}"
                );
            }
            other => panic!("expected a trailing-bytes codec error, got: {other:?}"),
        }
        Ok(())
    }

    #[test]
    fn read_message_accepts_exact_payload() {
        // A normally-framed message should decode without error.
        let msg = ClientMessage::TerminalHello {
            cols: 80,
            rows: 24,
            cell_width_px: 8,
            cell_height_px: 16,
            pixel_mouse: false,
        };
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).expect("test precondition");
        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(msg, decoded);
    }

    #[test]
    fn write_message_rejects_oversized_payload() {
        // Input is variant 1 (one byte) followed by a 3-byte varint length for
        // payloads this size, so `data` of MAX_FRAME_SIZE - 4 bytes encodes to
        // exactly MAX_FRAME_SIZE.
        let envelope = 4;
        let at_limit = ClientMessage::Input {
            data: vec![b'x'; MAX_FRAME_SIZE - envelope],
        };
        assert_eq!(
            codec::encoded_len(&at_limit).expect("test precondition"),
            MAX_FRAME_SIZE
        );
        let mut buf = Vec::new();
        write_message(&mut buf, &at_limit).expect("a frame at the cap is accepted");
        let decoded: ClientMessage =
            read_message(&mut buf.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(decoded, at_limit);

        let over_limit = ClientMessage::Input {
            data: vec![b'x'; MAX_FRAME_SIZE - envelope + 1],
        };
        let mut buf = Vec::new();
        match write_message(&mut buf, &over_limit) {
            Err(FramingError::Oversized { claimed, max }) => {
                assert_eq!(claimed, MAX_FRAME_SIZE + 1);
                assert_eq!(max, MAX_FRAME_SIZE);
            }
            other => panic!("expected Oversized, got {other:?}"),
        }
        assert!(buf.is_empty(), "nothing is written for a rejected frame");
    }

    #[test]
    fn encode_frame_matches_write_message_and_enforces_the_cap() {
        let msg = ServerMessage::WindowTitle {
            title: Some("frame".into()),
        };
        let frame = encode_frame(&msg).expect("test precondition");
        let mut written = Vec::new();
        write_message(&mut written, &msg).expect("test precondition");
        assert_eq!(frame, written);
        let decoded: ServerMessage =
            read_message(&mut frame.as_slice(), MAX_FRAME_SIZE).expect("test precondition");
        assert_eq!(decoded, msg);

        let over_limit = ClientMessage::Input {
            data: vec![b'x'; MAX_FRAME_SIZE],
        };
        assert!(matches!(
            encode_frame(&over_limit),
            Err(FramingError::Oversized { max, .. }) if max == MAX_FRAME_SIZE
        ));
    }

    // ---- Unix socketpair integration test ----

    #[test]
    fn framing_over_unix_socketpair() {
        use std::os::unix::net::UnixStream;

        let (mut a, mut b) = UnixStream::pair().expect("socketpair");

        let messages = vec![
            ClientMessage::TerminalHello {
                cols: 200,
                rows: 60,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: true,
            },
            ClientMessage::Input {
                data: b"hello world".to_vec(),
            },
            ClientMessage::Resize {
                cols: 100,
                rows: 30,
                cell_width_px: 8,
                cell_height_px: 16,
                pixel_mouse: true,
            },
            ClientMessage::Detach,
        ];

        // Set non-blocking so we can write and read in the same test.
        a.set_nonblocking(false).expect("test precondition");
        b.set_nonblocking(false).expect("test precondition");

        for msg in &messages {
            write_message(&mut a, msg).expect("test precondition");
        }

        for expected in &messages {
            let decoded: ClientMessage =
                read_message(&mut b, MAX_FRAME_SIZE).expect("test precondition");
            assert_eq!(*expected, decoded);
        }
    }

    // ---- Helper: chunked reader for simulating partial reads ----

    /// A `Read` wrapper that yields at most `chunk_size` bytes per `read()` call,
    /// simulating partial reads on a real socket.
    struct ChunkedReader {
        data: Vec<u8>,
        pos: usize,
        chunk_size: usize,
    }

    impl ChunkedReader {
        fn new(data: Vec<u8>, chunk_size: usize) -> Self {
            Self {
                data,
                pos: 0,
                chunk_size,
            }
        }
    }

    impl Read for ChunkedReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.pos >= self.data.len() {
                return Ok(0);
            }
            let remaining = self.data.len() - self.pos;
            let to_read = buf.len().min(remaining).min(self.chunk_size);
            buf[..to_read].copy_from_slice(&self.data[self.pos..self.pos + to_read]);
            self.pos += to_read;
            Ok(to_read)
        }
    }
}
