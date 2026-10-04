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
    /// The first column of a wide glyph: the cell's symbol is drawn across this
    /// column and the next, which holds the glyph's `WideTail`.
    WideLead,
    /// The second column of a wide glyph. Its symbol is empty and the client
    /// draws nothing for it. A tail is only half of a pair: whether a lead
    /// precedes it is a row property, and a changed span may begin with a tail
    /// whose lead is in the baseline.
    WideTail,
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
/// The fields are private: a frame is built by [`FrameData::new`] (or
/// [`FrameData::blank`]) and stays shape-valid for its whole life. Its size
/// is within the surface budget, its cell count is `width * height`, and its
/// hyperlink table is within budget with every cell link pointing into it.
/// Mutation goes through `cells_mut` (the cell count cannot change), the
/// cursor setter and the hyperlink table methods. A cell link set through
/// `cells_mut` is not rechecked until [`FrameData::validate`]; the surface
/// decoder runs it at the wire boundary.
///
/// Deserialization validates through [`FrameData::new`], so a frame that does
/// not fit its own grid fails the decode. Surface-level errors that need the
/// enclosing boot, projection and surface identities come from the decoder's
/// own checks of update spans.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "FrameWire")]
pub struct FrameData {
    /// Cells in row-major order. Length equals `width * height`.
    #[serde(serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_CELLS, _, _>")]
    cells: Vec<CellData>,
    /// Frame width in columns.
    width: u16,
    /// Frame height in rows.
    height: u16,
    /// Cursor state for this frame, if applicable.
    cursor: Option<CursorState>,
    /// OSC 8 hyperlink URIs referenced by cells.
    #[serde(serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_HYPERLINKS, _, _>")]
    hyperlinks: Vec<String>,
}

/// The positional wire shape of a [`FrameData`], checked on its way in.
#[derive(Deserialize)]
struct FrameWire {
    #[serde(deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_CELLS, _, _>")]
    cells: Vec<CellData>,
    width: u16,
    height: u16,
    cursor: Option<CursorState>,
    #[serde(deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_HYPERLINKS, _, _>")]
    hyperlinks: Vec<String>,
}

impl TryFrom<FrameWire> for FrameData {
    type Error = FrameGridError;

    fn try_from(wire: FrameWire) -> Result<Self, Self::Error> {
        Self::new(
            wire.cells,
            wire.width,
            wire.height,
            wire.cursor,
            wire.hyperlinks,
        )
    }
}

/// A shape-checked borrowed grid over cells that are not (or not yet) a [`FrameData`]:
/// the retained baseline of a surface decoder, or a patch producer's view of one.
/// A borrow cannot outlive a mutation, so the view never claims more than the shape
/// it checked.
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

impl std::fmt::Display for FrameGridError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::InvalidDimensions => "pane surface dimensions exceed the limit",
            Self::InvalidCellCount => "pane surface cell count does not match its size",
            Self::InvalidHyperlink => "surface has an invalid hyperlink table or index",
        })
    }
}

impl std::error::Error for FrameGridError {}

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
    /// The one validating constructor. Fails when the size is outside the surface
    /// budget, `cells` is not `width * height` long, the link table is over its
    /// budget, or a cell links outside the table.
    pub fn new(
        cells: Vec<CellData>,
        width: u16,
        height: u16,
        cursor: Option<CursorState>,
        hyperlinks: Vec<String>,
    ) -> Result<Self, FrameGridError> {
        FrameGrid::new(&cells, width, height)?;
        if hyperlinks.len() > MAX_SURFACE_HYPERLINKS {
            return Err(FrameGridError::InvalidHyperlink);
        }
        validate_cell_hyperlinks(&cells, &hyperlinks)?;
        Ok(Self {
            cells,
            width,
            height,
            cursor,
            hyperlinks,
        })
    }

    /// A `width` by `height` frame of blank cells, with no cursor or links.
    /// Fails when the size is outside the surface budget.
    pub fn blank(width: u16, height: u16) -> Result<Self, FrameGridError> {
        let count =
            crate::surface_grid_size(width, height).ok_or(FrameGridError::InvalidDimensions)?;
        Ok(Self {
            cells: vec![CellData::blank(); count],
            width,
            height,
            cursor: None,
            hyperlinks: Vec::new(),
        })
    }

    /// Frame width in columns.
    pub fn width(&self) -> u16 {
        self.width
    }

    /// Frame height in rows.
    pub fn height(&self) -> u16 {
        self.height
    }

    /// Cells in row-major order: exactly `width * height` of them.
    pub fn cells(&self) -> &[CellData] {
        &self.cells
    }

    /// The cells for in-place edits. The cell count cannot change through this
    /// borrow. A link set on a cell must index the table (`validate` checks).
    ///
    /// Not made link-safe by construction on purpose: `CellData.hyperlink` is a
    /// plain wire field, so a guard type would wrap a rule that already holds.
    /// Production writers that set a link intern it in the table first, the
    /// others only clear links or restyle, and the decoder and `Canvas::new`
    /// validate at the boundaries. A stray index is dropped by readers (they
    /// use `.get`) and never panics.
    pub fn cells_mut(&mut self) -> &mut [CellData] {
        &mut self.cells
    }

    /// The cell at column `x`, row `y`, if the frame has one.
    pub fn cell(&self, x: u16, y: u16) -> Option<&CellData> {
        (x < self.width && y < self.height)
            .then(|| {
                self.cells
                    .get(usize::from(y) * usize::from(self.width) + usize::from(x))
            })
            .flatten()
    }

    pub fn cursor(&self) -> Option<&CursorState> {
        self.cursor.as_ref()
    }

    pub fn set_cursor(&mut self, cursor: Option<CursorState>) {
        self.cursor = cursor;
    }

    /// OSC 8 hyperlink URIs referenced by cells.
    pub fn hyperlinks(&self) -> &[String] {
        &self.hyperlinks
    }

    /// Replaces the link table. Fails, leaving the frame unchanged, when the table
    /// is over its budget or a cell links past its end.
    pub fn set_hyperlinks(&mut self, hyperlinks: Vec<String>) -> Result<(), FrameGridError> {
        if hyperlinks.len() > MAX_SURFACE_HYPERLINKS {
            return Err(FrameGridError::InvalidHyperlink);
        }
        validate_cell_hyperlinks(&self.cells, &hyperlinks)?;
        self.hyperlinks = hyperlinks;
        Ok(())
    }

    /// Appends `uri` to the link table without looking for it first and returns
    /// its index, or `None` once the table is full.
    pub fn push_hyperlink(&mut self, uri: String) -> Option<u32> {
        if self.hyperlinks.len() >= MAX_SURFACE_HYPERLINKS {
            return None;
        }
        let index = u32::try_from(self.hyperlinks.len()).ok()?;
        self.hyperlinks.push(uri);
        Some(index)
    }

    pub fn grid(&self) -> Result<FrameGrid<'_>, FrameGridError> {
        FrameGrid::new(&self.cells, self.width, self.height)
    }

    /// Rechecks the link indices that `cells_mut` edits could have broken. The
    /// shape cannot be wrong.
    pub fn validate(&self) -> Result<(), FrameGridError> {
        self.grid()?.validate_hyperlinks(&self.hyperlinks)
    }

    /// The index of `uri` in this frame's link table, adding it when absent.
    /// `None` once the table is full: the cell then simply carries no link,
    /// rather than the frame growing past what the wire accepts.
    /// A match at the table tail makes repeated calls for adjacent hyperlink
    /// cells constant-time. Other existing URIs are found with a linear scan.
    /// The pane renderer uses a per-render index for distinct links, which it
    /// must keep in step with `push_hyperlink`.
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
    fn constructor_distinguishes_shape_and_hyperlink_failures() {
        let blank = || vec![CellData::blank(); 2];
        assert!(FrameData::new(blank(), 2, 1, None, Vec::new()).is_ok());
        assert_eq!(
            FrameData::new(blank(), 3, 1, None, Vec::new()),
            Err(FrameGridError::InvalidCellCount)
        );
        assert_eq!(
            FrameData::new(Vec::new(), u16::MAX, u16::MAX, None, Vec::new()),
            Err(FrameGridError::InvalidDimensions)
        );
        let mut linked = blank();
        linked[0].hyperlink = Some(0);
        assert_eq!(
            FrameData::new(linked.clone(), 2, 1, None, Vec::new()),
            Err(FrameGridError::InvalidHyperlink)
        );
        assert!(FrameData::new(linked, 2, 1, None, vec!["https://example.test".into()]).is_ok());
        assert_eq!(
            FrameData::new(
                blank(),
                2,
                1,
                None,
                vec![String::new(); MAX_SURFACE_HYPERLINKS + 1]
            ),
            Err(FrameGridError::InvalidHyperlink)
        );
        assert_eq!(
            FrameData::blank(u16::MAX, u16::MAX),
            Err(FrameGridError::InvalidDimensions)
        );
    }

    #[test]
    fn link_edits_keep_the_table_valid() {
        let mut frame = FrameData::blank(2, 1).expect("small frame");
        frame.cells_mut()[0].hyperlink = Some(0);
        // A shape-checked composition borrow does not require finished link remapping.
        assert!(frame.grid().is_ok());
        assert_eq!(frame.validate(), Err(FrameGridError::InvalidHyperlink));
        assert_eq!(frame.push_hyperlink("https://example.test".into()), Some(0));
        assert!(frame.validate().is_ok());
        // A table that would orphan a cell's link is refused and changes nothing.
        assert_eq!(
            frame.set_hyperlinks(Vec::new()),
            Err(FrameGridError::InvalidHyperlink)
        );
        assert_eq!(frame.hyperlinks().len(), 1);
    }

    #[test]
    fn deserialization_validates_the_grid() {
        let frame = FrameData::blank(2, 1).expect("small frame");
        let bytes = crate::codec::to_vec(&frame).expect("frame encoding");
        let decoded: FrameData =
            crate::codec::from_slice_exact(&bytes).expect("valid frame decodes");
        assert_eq!(decoded, frame);
        // The positional encoding is a tuple of the fields: a width of three
        // leaves the grid one cell short.
        let short = (
            vec![CellData::blank(); 2],
            3u16,
            1u16,
            None::<CursorState>,
            Vec::<String>::new(),
        );
        let bytes = crate::codec::to_vec(&short).expect("tuple encoding");
        assert!(crate::codec::from_slice_exact::<FrameData>(&bytes).is_err());
    }

    #[test]
    fn grid_width_wire_value_uses_one_byte() {
        for grid_width in [
            GridCellWidth::Grapheme,
            GridCellWidth::One,
            GridCellWidth::WideLead,
            GridCellWidth::WideTail,
        ] {
            assert_eq!(
                crate::codec::encoded_len(&grid_width).expect("width encoding"),
                1
            );
        }
    }
}
