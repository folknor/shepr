use super::*;
use serde::{Deserialize, Serialize};

/// A single cell in a rendered frame, serialized independently from ratatui's
/// `Cell` type to keep the wire protocol semantic and explicit.
#[derive(Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellData {
    /// Grapheme cluster displayed in this cell (usually 1-2 chars).
    pub symbol: String,
    /// Grid width of this cell, or grapheme-based width for client chrome.
    pub grid_width: GridCellWidth,
    /// Foreground color.
    pub fg: WireColor,
    /// Background color.
    pub bg: WireColor,
    /// Style flags and underline shape.
    pub style: WireStyle,
    /// Whether this cell should be skipped during diff-based rendering.
    pub skip: bool,
    /// Index into `FrameData::hyperlinks` for this cell's OSC 8 target, if any.
    pub hyperlink: Option<u32>,
}

/// Width semantics for one cell in the terminal grid.
///
/// Variant order is part of the positional wire format. Pane renderers report
/// the terminal grid width directly; Ratatui chrome keeps grapheme sizing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GridCellWidth {
    /// Use the displayed grapheme's width, as Ratatui chrome does.
    Grapheme,
    /// The cell occupies one terminal grid column.
    One,
    /// The cell occupies two terminal grid columns.
    Two,
}

impl CellData {
    /// An unstyled space: the cell of an empty surface.
    pub fn blank() -> Self {
        Self {
            symbol: " ".to_owned(),
            grid_width: GridCellWidth::Grapheme,
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: WireStyle::default(),
            skip: false,
            hyperlink: None,
        }
    }
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

/// Cursor shape encoded as a DECSCUSR parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum CursorShapeParam {
    Default = 0,
    BlinkingBlock = 1,
    SteadyBlock = 2,
    BlinkingUnderline = 3,
    SteadyUnderline = 4,
    BlinkingBar = 5,
    SteadyBar = 6,
}

impl CursorShapeParam {
    pub fn from_decscusr(value: u8) -> Self {
        match value {
            1 => Self::BlinkingBlock,
            2 => Self::SteadyBlock,
            3 => Self::BlinkingUnderline,
            4 => Self::SteadyUnderline,
            5 => Self::BlinkingBar,
            6 => Self::SteadyBar,
            _ => Self::Default,
        }
    }
}

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
    pub shape: CursorShapeParam,
}

/// A rendered frame to be displayed by the client.
///
/// This is also the mutable construction buffer for renderers and composition:
/// cells and hyperlink tables can be populated separately. Use `grid` for a
/// shape-checked borrow and `validate` at a complete-frame boundary. Validation
/// stays in the surface decoder rather than serde so errors retain the enclosing
/// boot, projection and surface identities needed to diagnose a rejected update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FrameData {
    /// Cells in row-major order. Length must equal `width * height`.
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_CELLS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_CELLS, _, _>"
    )]
    pub cells: Vec<CellData>,
    /// Frame width in columns.
    pub width: u16,
    /// Frame height in rows.
    pub height: u16,
    /// Cursor state for this frame, if applicable.
    pub cursor: Option<CursorState>,
    /// OSC 8 hyperlink URIs referenced by cells.
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_HYPERLINKS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_HYPERLINKS, _, _>"
    )]
    pub hyperlinks: Vec<String>,
}

/// A shape-checked borrowed grid. Mutable frame construction stays separate: renderers
/// build and compose cells and hyperlink tables in several steps. A borrow cannot outlive
/// a mutation, so this view never claims those in-progress frames stay valid.
pub struct FrameGrid<'a> {
    cells: &'a [CellData],
    width: u16,
    height: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameGridError {
    InvalidDimensions,
    InvalidCellCount,
    InvalidHyperlink,
}

impl<'a> FrameGrid<'a> {
    pub fn new(cells: &'a [CellData], width: u16, height: u16) -> Result<Self, FrameGridError> {
        let expected =
            crate::surface_grid_size(width, height).ok_or(FrameGridError::InvalidDimensions)?;
        if cells.len() != expected {
            return Err(FrameGridError::InvalidCellCount);
        }
        Ok(Self {
            cells,
            width,
            height,
        })
    }

    pub fn cells(&self) -> &'a [CellData] {
        self.cells
    }
    pub fn width(&self) -> u16 {
        self.width
    }
    pub fn height(&self) -> u16 {
        self.height
    }

    pub fn validate_hyperlinks(&self, hyperlinks: &[String]) -> Result<(), FrameGridError> {
        validate_cell_hyperlinks(self.cells, hyperlinks)
    }
}

/// Shared by complete grids and changed spans; spans need not contain complete wide pairs.
pub fn validate_cell_hyperlinks(
    cells: &[CellData],
    hyperlinks: &[String],
) -> Result<(), FrameGridError> {
    if cells.iter().any(|cell| {
        cell.hyperlink.is_some_and(|index| {
            !usize::try_from(index).is_ok_and(|index| index < hyperlinks.len())
        })
    }) {
        return Err(FrameGridError::InvalidHyperlink);
    }
    Ok(())
}

impl FrameData {
    pub fn grid(&self) -> Result<FrameGrid<'_>, FrameGridError> {
        FrameGrid::new(&self.cells, self.width, self.height)
    }

    pub fn validate(&self) -> Result<(), FrameGridError> {
        self.grid()?.validate_hyperlinks(&self.hyperlinks)
    }

    /// A `width` by `height` frame of blank cells, with no cursor or links.
    pub fn blank(width: u16, height: u16) -> Self {
        Self {
            cells: vec![CellData::blank(); usize::from(width) * usize::from(height)],
            width,
            height,
            cursor: None,
            hyperlinks: Vec::new(),
        }
    }

    /// The index of `uri` in this frame's link table, adding it when absent.
    /// `None` once the table is full: the cell then simply carries no link,
    /// rather than the frame growing past what the wire accepts.
    /// A match at the table tail makes repeated calls for adjacent hyperlink
    /// cells constant-time. Other existing URIs are found with a linear scan.
    /// The pane renderer uses a per-render index for distinct links. A cache
    /// cannot safely live in `FrameData` while this public vector can be edited
    /// or replaced directly, so a renderer that owns a frame's construction
    /// must own and update any scoped index alongside the vector.
    pub fn intern_hyperlink(&mut self, uri: &str) -> Option<u32> {
        if self.hyperlinks.last().is_some_and(|known| known == uri) {
            return self
                .hyperlinks
                .len()
                .checked_sub(1)
                .and_then(|index| u32::try_from(index).ok());
        }
        if let Some(index) = self.hyperlinks.iter().position(|known| known == uri) {
            return u32::try_from(index).ok();
        }
        if self.hyperlinks.len() >= MAX_SURFACE_HYPERLINKS {
            return None;
        }
        let index = u32::try_from(self.hyperlinks.len()).ok()?;
        self.hyperlinks.push(uri.to_owned());
        Some(index)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_validation_distinguishes_shape_and_hyperlink_failures() {
        let mut frame = FrameData::blank(2, 1);
        assert!(frame.validate().is_ok());
        frame.cells[0].hyperlink = Some(0);
        assert_eq!(frame.validate(), Err(FrameGridError::InvalidHyperlink));
        // A shape-checked composition borrow does not require finished link remapping.
        assert!(frame.grid().is_ok());
        frame.hyperlinks.push("https://example.test".into());
        assert!(frame.validate().is_ok());
        frame.cells.pop();
        assert_eq!(frame.validate(), Err(FrameGridError::InvalidCellCount));
        frame.width = u16::MAX;
        assert_eq!(frame.validate(), Err(FrameGridError::InvalidDimensions));
    }

    #[test]
    fn grid_width_wire_value_uses_one_byte() {
        for grid_width in [
            GridCellWidth::Grapheme,
            GridCellWidth::One,
            GridCellWidth::Two,
        ] {
            assert_eq!(
                crate::codec::encoded_len(&grid_width).expect("width encoding"),
                1
            );
        }
    }
}
