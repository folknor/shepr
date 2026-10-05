//! Conversion from what ratatui renderers draw (chrome, scratch buffers) to
//! semantic wire frames. Pane cells do not use it: they are written straight
//! into `FrameData`.
//!
//! The wire types live in `shepr-protocol`, which knows nothing of ratatui, so
//! the conversions are extension traits on them; bring the trait into scope to
//! call `WireColor::from_ratatui` and the rest.

use std::collections::HashMap;

use shepr_protocol::{
    CellData, CompactString, CursorState, FrameData, FrameGridError, GridCellWidth, SurfaceRect,
    WireColor, WireStyle, WireStyleFlags,
};

/// Conversion between a wire color and a ratatui color. Lossless both ways,
/// except that a chrome role has no ratatui form and reads as `Reset`:
/// composition resolves roles before anything is drawn.
pub trait WireColorExt: Sized {
    fn from_ratatui(color: ratatui::style::Color) -> Self;
    fn to_ratatui(self) -> ratatui::style::Color;
}

impl WireColorExt for WireColor {
    fn from_ratatui(color: ratatui::style::Color) -> Self {
        match color {
            ratatui::style::Color::Reset => Self::Reset,
            ratatui::style::Color::Black => Self::Black,
            ratatui::style::Color::Red => Self::Red,
            ratatui::style::Color::Green => Self::Green,
            ratatui::style::Color::Yellow => Self::Yellow,
            ratatui::style::Color::Blue => Self::Blue,
            ratatui::style::Color::Magenta => Self::Magenta,
            ratatui::style::Color::Cyan => Self::Cyan,
            ratatui::style::Color::Gray => Self::Gray,
            ratatui::style::Color::DarkGray => Self::DarkGray,
            ratatui::style::Color::LightRed => Self::LightRed,
            ratatui::style::Color::LightGreen => Self::LightGreen,
            ratatui::style::Color::LightYellow => Self::LightYellow,
            ratatui::style::Color::LightBlue => Self::LightBlue,
            ratatui::style::Color::LightMagenta => Self::LightMagenta,
            ratatui::style::Color::LightCyan => Self::LightCyan,
            ratatui::style::Color::White => Self::White,
            ratatui::style::Color::Indexed(index) => Self::Indexed(index),
            ratatui::style::Color::Rgb(red, green, blue) => Self::Rgb(red, green, blue),
        }
    }

    fn to_ratatui(self) -> ratatui::style::Color {
        match self {
            Self::Reset | Self::Chrome(_) => ratatui::style::Color::Reset,
            Self::Black => ratatui::style::Color::Black,
            Self::Red => ratatui::style::Color::Red,
            Self::Green => ratatui::style::Color::Green,
            Self::Yellow => ratatui::style::Color::Yellow,
            Self::Blue => ratatui::style::Color::Blue,
            Self::Magenta => ratatui::style::Color::Magenta,
            Self::Cyan => ratatui::style::Color::Cyan,
            Self::Gray => ratatui::style::Color::Gray,
            Self::DarkGray => ratatui::style::Color::DarkGray,
            Self::LightRed => ratatui::style::Color::LightRed,
            Self::LightGreen => ratatui::style::Color::LightGreen,
            Self::LightYellow => ratatui::style::Color::LightYellow,
            Self::LightBlue => ratatui::style::Color::LightBlue,
            Self::LightMagenta => ratatui::style::Color::LightMagenta,
            Self::LightCyan => ratatui::style::Color::LightCyan,
            Self::White => ratatui::style::Color::White,
            Self::Indexed(index) => ratatui::style::Color::Indexed(index),
            Self::Rgb(red, green, blue) => ratatui::style::Color::Rgb(red, green, blue),
        }
    }
}

/// The wire style of a ratatui modifier.
pub trait WireStyleExt: Sized {
    /// The style of a ratatui modifier. Ratatui has no underline shape, so
    /// `UNDERLINED` reads as a single underline and nothing else can be
    /// recovered: use this for cells a ratatui renderer drew (chrome, scratch
    /// buffers), never for pane cells, which are written to the wire with
    /// their typed shape.
    fn from_ratatui_modifier(modifier: ratatui::style::Modifier) -> Self;
}

impl WireStyleExt for WireStyle {
    fn from_ratatui_modifier(modifier: ratatui::style::Modifier) -> Self {
        use ratatui::style::Modifier;

        const FLAGS: [(Modifier, WireStyleFlags); 8] = [
            (Modifier::BOLD, WireStyleFlags::BOLD),
            (Modifier::DIM, WireStyleFlags::DIM),
            (Modifier::ITALIC, WireStyleFlags::ITALIC),
            (Modifier::SLOW_BLINK, WireStyleFlags::SLOW_BLINK),
            (Modifier::RAPID_BLINK, WireStyleFlags::RAPID_BLINK),
            (Modifier::REVERSED, WireStyleFlags::REVERSED),
            (Modifier::HIDDEN, WireStyleFlags::HIDDEN),
            (Modifier::CROSSED_OUT, WireStyleFlags::CROSSED_OUT),
        ];
        let flags = FLAGS
            .into_iter()
            .filter(|(source, _)| modifier.contains(*source))
            .fold(WireStyleFlags::default(), |flags, (_, flag)| {
                flags.union(flag)
            });
        let underline = if modifier.contains(Modifier::UNDERLINED) {
            shepr_term::UnderlineStyle::Single
        } else {
            shepr_term::UnderlineStyle::None
        };
        Self { flags, underline }
    }
}

/// The wire cell of a cell a ratatui renderer drew.
pub trait CellDataExt: Sized {
    /// A cell a ratatui renderer drew. Lossless for everything the wire cell
    /// carries; an underline is `Single` (see
    /// [`WireStyleExt::from_ratatui_modifier`]). Ratatui's diff option (the
    /// `Skip` hint for its own buffer diff) has no wire counterpart and is
    /// dropped here. Pane cells are never drawn through ratatui, so none reach
    /// this.
    fn from_ratatui_cell(cell: &ratatui::buffer::Cell) -> Self;

    /// Makes `self` the cell [`Self::from_ratatui_cell`] would build, reusing
    /// its symbol buffer, so a frame cell overwritten in place allocates only
    /// when the new symbol outgrows the old one.
    fn assign_ratatui_cell(&mut self, cell: &ratatui::buffer::Cell);

    /// Whether `self` equals the cell [`Self::from_ratatui_cell`] would build,
    /// decided without building it, so a diff against a frame allocates only
    /// for the cells that differ.
    fn matches_ratatui_cell(&self, cell: &ratatui::buffer::Cell) -> bool;
}

impl CellDataExt for CellData {
    fn from_ratatui_cell(cell: &ratatui::buffer::Cell) -> Self {
        Self {
            symbol: CompactString::new(cell.symbol()),
            grid_width: GridCellWidth::Grapheme,
            fg: WireColor::from_ratatui(cell.fg),
            bg: WireColor::from_ratatui(cell.bg),
            style: WireStyle::from_ratatui_modifier(cell.modifier),
            hyperlink: None,
        }
    }

    fn assign_ratatui_cell(&mut self, cell: &ratatui::buffer::Cell) {
        let mut symbol = std::mem::take(&mut self.symbol);
        symbol.clear();
        symbol.push_str(cell.symbol());
        *self = Self {
            symbol,
            grid_width: GridCellWidth::Grapheme,
            fg: WireColor::from_ratatui(cell.fg),
            bg: WireColor::from_ratatui(cell.bg),
            style: WireStyle::from_ratatui_modifier(cell.modifier),
            hyperlink: None,
        };
    }

    fn matches_ratatui_cell(&self, cell: &ratatui::buffer::Cell) -> bool {
        // Destructured so a new wire field cannot be silently left out.
        let Self {
            symbol,
            grid_width,
            fg,
            bg,
            style,
            hyperlink,
        } = self;
        symbol == cell.symbol()
            && *grid_width == GridCellWidth::Grapheme
            && *fg == WireColor::from_ratatui(cell.fg)
            && *bg == WireColor::from_ratatui(cell.bg)
            && *style == WireStyle::from_ratatui_modifier(cell.modifier)
            && hyperlink.is_none()
    }
}

/// The wire frame of a whole ratatui buffer.
pub trait FrameDataExt: Sized {
    /// The frame a ratatui buffer holds, with `cursor`. `hyperlinks` names
    /// `((x, y), symbol, uri)` triples; a cell takes its link only while it
    /// still shows that symbol. Fails when the buffer is not a grid the wire
    /// budget admits.
    fn from_ratatui_buffer_with_hyperlinks(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
        hyperlinks: &[((u16, u16), String, String)],
    ) -> Result<Self, FrameGridError>;
}

impl FrameDataExt for FrameData {
    fn from_ratatui_buffer_with_hyperlinks(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
        hyperlinks: &[((u16, u16), String, String)],
    ) -> Result<Self, FrameGridError> {
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
        let row_len = usize::from(width).max(crate::limits::MIN_BUFFER_ROW_LEN);
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

        Self::new(cells, width, height, cursor, hyperlink_uris)
    }
}

/// The wire rectangle of a ratatui rectangle.
pub fn surface_rect(rect: ratatui::layout::Rect) -> SurfaceRect {
    SurfaceRect {
        x: rect.x,
        y: rect.y,
        width: rect.width,
        height: rect.height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    #[test]
    fn in_place_conversion_and_comparison_agree_with_from_ratatui_cell() {
        let mut source = ratatui::buffer::Cell::new("\u{2503}");
        source.fg = Color::Rgb(1, 2, 3);
        source.bg = Color::Indexed(4);
        source.modifier = Modifier::BOLD | Modifier::UNDERLINED;
        let built = CellData::from_ratatui_cell(&source);
        assert!(built.matches_ratatui_cell(&source));

        let mut assigned = CellData {
            symbol: "a previous symbol longer than inline".into(),
            grid_width: GridCellWidth::WideLead,
            fg: WireColor::Red,
            hyperlink: Some(0),
            ..CellData::blank()
        };
        assert!(!assigned.matches_ratatui_cell(&source));
        assigned.assign_ratatui_cell(&source);
        assert_eq!(assigned, built);

        // Each field the wire cell carries decides the comparison.
        let differs = |change: fn(&mut CellData)| {
            let mut cell = built.clone();
            change(&mut cell);
            !cell.matches_ratatui_cell(&source)
        };
        assert!(differs(|cell| cell.symbol.push('x')));
        assert!(differs(|cell| cell.grid_width = GridCellWidth::One));
        assert!(differs(|cell| cell.fg = WireColor::Reset));
        assert!(differs(|cell| cell.bg = WireColor::Reset));
        assert!(differs(|cell| cell.style = WireStyle::default()));
        assert!(differs(|cell| cell.hyperlink = Some(0)));
    }

    #[test]
    fn frame_data_from_ratatui_buffer_keeps_cells_and_cursor() {
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
            shape: shepr_protocol::CursorShapeParam::Default,
        };
        let frame =
            FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, Some(cursor.clone()), &[])
                .expect("test precondition");

        // Verify frame dimensions.
        assert_eq!(frame.width(), 5);
        assert_eq!(frame.height(), 3);
        assert_eq!(frame.cells().len(), 15);
        assert_eq!(frame.cursor(), Some(&cursor));

        // Verify specific cells survived the conversion.
        assert_eq!(frame.cells()[0].symbol, "H");
        assert_eq!(frame.cells()[0].fg, WireColor::from_ratatui(Color::Red));
        assert!(frame.cells()[0].style.flags.contains(WireStyleFlags::BOLD));

        assert_eq!(frame.cells()[1].symbol, "i");
        assert_eq!(frame.cells()[1].fg, WireColor::from_ratatui(Color::Green));
        assert!(
            frame.cells()[1]
                .style
                .flags
                .contains(WireStyleFlags::ITALIC)
        );

        assert_eq!(frame.cells()[2].symbol, "!");
        assert_eq!(
            frame.cells()[2].fg,
            WireColor::from_ratatui(Color::Rgb(255, 128, 0))
        );
        assert_eq!(
            frame.cells()[2].bg,
            WireColor::from_ratatui(Color::Indexed(220))
        );

        let with_links = FrameData::from_ratatui_buffer_with_hyperlinks(
            &buffer,
            None,
            &[((1, 0), "i".to_owned(), "https://example.com".to_owned())],
        )
        .expect("test precondition");
        assert_eq!(with_links.cells()[1].hyperlink, Some(0));
        assert_eq!(with_links.hyperlinks(), ["https://example.com".to_owned()]);
    }

    #[test]
    fn a_buffer_outside_the_wire_budget_is_refused() {
        let buffer = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 0, 0));
        assert_eq!(
            FrameData::from_ratatui_buffer_with_hyperlinks(&buffer, None, &[]),
            Err(FrameGridError::InvalidDimensions)
        );
    }

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
                WireColor::from_ratatui(c).to_ratatui(),
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
                WireColor::from_ratatui(c).to_ratatui(),
                c,
                "roundtrip failed for Indexed({i})"
            );
        }
    }

    #[test]
    fn color_roundtrip_rgb() {
        let c = Color::Rgb(0xAB, 0xCD, 0xEF);
        assert_eq!(WireColor::from_ratatui(c).to_ratatui(), c);

        let c = Color::Rgb(0, 0, 0);
        assert_eq!(WireColor::from_ratatui(c).to_ratatui(), c);

        let c = Color::Rgb(255, 255, 255);
        assert_eq!(WireColor::from_ratatui(c).to_ratatui(), c);
    }

    #[test]
    fn wire_style_from_ratatui_modifier_maps_flags_and_a_single_underline() {
        let cases = [
            (Modifier::BOLD, WireStyleFlags::BOLD),
            (Modifier::ITALIC, WireStyleFlags::ITALIC),
            (Modifier::REVERSED, WireStyleFlags::REVERSED),
            (Modifier::DIM, WireStyleFlags::DIM),
            (Modifier::SLOW_BLINK, WireStyleFlags::SLOW_BLINK),
            (Modifier::RAPID_BLINK, WireStyleFlags::RAPID_BLINK),
            (Modifier::HIDDEN, WireStyleFlags::HIDDEN),
            (Modifier::CROSSED_OUT, WireStyleFlags::CROSSED_OUT),
            (
                Modifier::BOLD | Modifier::ITALIC,
                WireStyleFlags::BOLD.union(WireStyleFlags::ITALIC),
            ),
            (Modifier::empty(), WireStyleFlags::default()),
        ];
        for (modifier, flags) in cases {
            let style = WireStyle::from_ratatui_modifier(modifier);
            assert_eq!(style.flags, flags, "{modifier:?}");
            assert_eq!(style.underline, shepr_term::UnderlineStyle::None);
        }
        let underlined = WireStyle::from_ratatui_modifier(Modifier::UNDERLINED | Modifier::BOLD);
        assert_eq!(underlined.underline, shepr_term::UnderlineStyle::Single);
        assert_eq!(underlined.flags, WireStyleFlags::BOLD);
    }
}
