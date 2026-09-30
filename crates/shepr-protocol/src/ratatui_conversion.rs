//! Conversion from what ratatui renderers draw (chrome, scratch buffers) to
//! semantic wire frames. Pane cells do not use it: they are written straight
//! into `FrameData`.

use crate::{CellData, CursorState, FrameData, SurfaceRect, WireColor, WireStyle, WireStyleFlags};
use std::collections::HashMap;

impl WireColor {
    pub fn from_ratatui(color: ratatui::style::Color) -> Self {
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

    pub fn to_ratatui(self) -> ratatui::style::Color {
        match self {
            Self::Reset => ratatui::style::Color::Reset,
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

impl WireStyle {
    /// The style of a ratatui modifier. Ratatui has no underline shape, so
    /// `UNDERLINED` reads as a single underline and nothing else can be
    /// recovered: use this for cells a ratatui renderer drew (chrome, scratch
    /// buffers), never for pane cells, which are written to the wire with
    /// their typed shape.
    pub fn from_ratatui_modifier(modifier: ratatui::style::Modifier) -> Self {
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
            shepr_vt::UnderlineStyle::Single
        } else {
            shepr_vt::UnderlineStyle::None
        };
        Self { flags, underline }
    }
}

impl CellData {
    /// A cell a ratatui renderer drew. Lossless for everything ratatui can
    /// express; an underline is `Single` (see [`WireStyle::from_ratatui_modifier`]).
    /// Pane cells are never drawn through ratatui, so none reach this.
    pub fn from_ratatui_cell(cell: &ratatui::buffer::Cell) -> Self {
        Self {
            symbol: cell.symbol().to_owned(),
            fg: WireColor::from_ratatui(cell.fg),
            bg: WireColor::from_ratatui(cell.bg),
            style: WireStyle::from_ratatui_modifier(cell.modifier),
            skip: cell.diff_option == ratatui::buffer::CellDiffOption::Skip,
            hyperlink: None,
        }
    }
}

impl FrameData {
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
        let row_len = usize::from(width).max(super::limits::MIN_BUFFER_ROW_LEN);
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

#[cfg(test)]
impl FrameData {
    /// Creates a `FrameData` from a ratatui `Buffer` and optional cursor.
    ///
    /// This converts ratatui's internal cell representation into the
    /// wire-protocol cell format. The conversion is lossless for all
    /// commonly used cell attributes.
    pub fn from_ratatui_buffer(
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) -> Self {
        Self::from_ratatui_buffer_with_hyperlinks(buffer, cursor, &[])
    }
}
