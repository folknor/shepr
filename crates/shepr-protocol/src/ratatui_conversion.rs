//! Conversion between semantic wire frames and ratatui buffers.

use crate::{CellData, CursorState, FrameData, SurfaceRect, WireColor, WireStyle, WireStyleFlags};
use crate::{RATATUI_UNDERLINE_STYLE_MASK, RATATUI_UNDERLINE_STYLE_SHIFT};
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
    pub fn from_ratatui_modifier(modifier: ratatui::style::Modifier) -> Self {
        use ratatui::style::Modifier;

        let underline = if modifier.contains(Modifier::UNDERLINED) {
            match (modifier.bits() & RATATUI_UNDERLINE_STYLE_MASK) >> RATATUI_UNDERLINE_STYLE_SHIFT
            {
                2 => shepr_vt::UnderlineStyle::Double,
                3 => shepr_vt::UnderlineStyle::Curly,
                4 => shepr_vt::UnderlineStyle::Dotted,
                5 => shepr_vt::UnderlineStyle::Dashed,
                _ => shepr_vt::UnderlineStyle::Single,
            }
        } else {
            shepr_vt::UnderlineStyle::None
        };

        Self {
            flags: {
                let mut flags = WireStyleFlags::default();
                if modifier.contains(Modifier::BOLD) {
                    flags = flags.union(WireStyleFlags::BOLD);
                }
                if modifier.contains(Modifier::DIM) {
                    flags = flags.union(WireStyleFlags::DIM);
                }
                if modifier.contains(Modifier::ITALIC) {
                    flags = flags.union(WireStyleFlags::ITALIC);
                }
                if modifier.contains(Modifier::SLOW_BLINK) {
                    flags = flags.union(WireStyleFlags::SLOW_BLINK);
                }
                if modifier.contains(Modifier::RAPID_BLINK) {
                    flags = flags.union(WireStyleFlags::RAPID_BLINK);
                }
                if modifier.contains(Modifier::REVERSED) {
                    flags = flags.union(WireStyleFlags::REVERSED);
                }
                if modifier.contains(Modifier::HIDDEN) {
                    flags = flags.union(WireStyleFlags::HIDDEN);
                }
                if modifier.contains(Modifier::CROSSED_OUT) {
                    flags = flags.union(WireStyleFlags::CROSSED_OUT);
                }
                flags
            },
            underline,
        }
    }

    pub fn to_ratatui_modifier(self) -> ratatui::style::Modifier {
        use ratatui::style::Modifier;

        let mut modifier = Modifier::empty();
        if self.flags.contains(WireStyleFlags::BOLD) {
            modifier |= Modifier::BOLD;
        }
        if self.flags.contains(WireStyleFlags::DIM) {
            modifier |= Modifier::DIM;
        }
        if self.flags.contains(WireStyleFlags::ITALIC) {
            modifier |= Modifier::ITALIC;
        }
        if self.flags.contains(WireStyleFlags::SLOW_BLINK) {
            modifier |= Modifier::SLOW_BLINK;
        }
        if self.flags.contains(WireStyleFlags::RAPID_BLINK) {
            modifier |= Modifier::RAPID_BLINK;
        }
        if self.flags.contains(WireStyleFlags::REVERSED) {
            modifier |= Modifier::REVERSED;
        }
        if self.flags.contains(WireStyleFlags::HIDDEN) {
            modifier |= Modifier::HIDDEN;
        }
        if self.flags.contains(WireStyleFlags::CROSSED_OUT) {
            modifier |= Modifier::CROSSED_OUT;
        }

        let underline_style = match self.underline {
            shepr_vt::UnderlineStyle::None => return modifier,
            shepr_vt::UnderlineStyle::Single => {
                modifier |= Modifier::UNDERLINED;
                return modifier;
            }
            shepr_vt::UnderlineStyle::Double => 2,
            shepr_vt::UnderlineStyle::Curly => 3,
            shepr_vt::UnderlineStyle::Dotted => 4,
            shepr_vt::UnderlineStyle::Dashed => 5,
        };
        modifier |= Modifier::UNDERLINED;
        modifier |= Modifier::from_bits_retain(underline_style << RATATUI_UNDERLINE_STYLE_SHIFT);
        modifier
    }
}

// Ratatui's Modifier has no underline-shape field. Preserve this metadata only
// while a frame crosses its in-memory Buffer during client composition; wire
// cells themselves carry the typed UnderlineStyle above.

impl CellData {
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

    pub fn replace_from_ratatui_buffer_preserving_effects(
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
    pub fn to_ratatui_buffer(&self) -> Option<ratatui::buffer::Buffer> {
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
                cell.fg = cell_data.fg.to_ratatui();
                cell.bg = cell_data.bg.to_ratatui();
                cell.modifier = cell_data.style.to_ratatui_modifier();
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
