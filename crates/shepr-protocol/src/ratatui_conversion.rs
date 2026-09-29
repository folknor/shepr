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

    /// Writes `buffer` back into this frame, keeping each cell's hyperlink while its
    /// symbol is unchanged. Only cells that differ from the buffer are rebuilt, so a
    /// buffer converted from this frame and touched by a few overlays costs a compare
    /// per untouched cell and no allocation. The result equals a full rebuild.
    pub fn replace_from_ratatui_buffer_preserving_effects(
        &mut self,
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) {
        let area = buffer.area;
        let comparable = self.width != 0
            && area.x == 0
            && area.y == 0
            && area.width == self.width
            && area.height == self.height
            && buffer.content.len() == self.cells.len();
        if !comparable {
            self.rebuild_preserving_hyperlinks(buffer, cursor);
            return;
        }
        let mut any_link = false;
        for (cell, buffer_cell) in self.cells.iter_mut().zip(buffer.content.iter()) {
            any_link |= cell.hyperlink.is_some();
            if cell.symbol == buffer_cell.symbol()
                && cell.fg == WireColor::from_ratatui(buffer_cell.fg)
                && cell.bg == WireColor::from_ratatui(buffer_cell.bg)
                && cell.style == WireStyle::from_ratatui_modifier(buffer_cell.modifier)
                && cell.skip == (buffer_cell.diff_option == ratatui::buffer::CellDiffOption::Skip)
            {
                continue;
            }
            // Same symbol keeps the link; a new symbol drops it.
            let hyperlink = cell
                .hyperlink
                .filter(|_| cell.symbol == buffer_cell.symbol());
            cell.symbol.clear();
            cell.symbol.push_str(buffer_cell.symbol());
            cell.fg = WireColor::from_ratatui(buffer_cell.fg);
            cell.bg = WireColor::from_ratatui(buffer_cell.bg);
            cell.style = WireStyle::from_ratatui_modifier(buffer_cell.modifier);
            cell.skip = buffer_cell.diff_option == ratatui::buffer::CellDiffOption::Skip;
            cell.hyperlink = hyperlink;
        }
        self.cursor = cursor;
        if any_link || !self.hyperlinks.is_empty() {
            self.canonicalize_hyperlinks();
        }
    }

    /// Renumbers hyperlinks in first-use order over the cells, merges equal URIs, drops
    /// unreferenced ones and clears indices that point nowhere.
    fn canonicalize_hyperlinks(&mut self) {
        let old = std::mem::take(&mut self.hyperlinks);
        let mut uris = Vec::<String>::new();
        let mut by_uri = HashMap::<&str, u32>::new();
        let mut remap = vec![None::<u32>; old.len()];
        for cell in &mut self.cells {
            let Some(old_index) = cell.hyperlink else {
                continue;
            };
            let old_index = old_index as usize;
            cell.hyperlink = match remap.get(old_index).copied() {
                None => None,
                Some(Some(new)) => Some(new),
                Some(None) => {
                    let uri = old[old_index].as_str();
                    let new = *by_uri.entry(uri).or_insert_with(|| {
                        let index = u32::try_from(uris.len()).unwrap_or(u32::MAX);
                        uris.push(uri.to_owned());
                        index
                    });
                    remap[old_index] = Some(new);
                    Some(new)
                }
            };
        }
        self.hyperlinks = uris;
    }

    /// Full rebuild of every cell, for a buffer that does not line up with this frame.
    fn rebuild_preserving_hyperlinks(
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

#[cfg(test)]
mod write_back_tests {
    use super::*;
    use ratatui::style::{Color, Modifier, Style};

    /// The full rebuild the diffing write-back replaced.
    fn full_rebuild(
        frame: &FrameData,
        buffer: &ratatui::buffer::Buffer,
        cursor: Option<CursorState>,
    ) -> FrameData {
        let hyperlinks = frame
            .cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                let uri = frame.hyperlinks.get(cell.hyperlink? as usize)?;
                let x = u16::try_from(index % usize::from(frame.width)).ok()?;
                let y = u16::try_from(index / usize::from(frame.width)).ok()?;
                Some(((x, y), cell.symbol.clone(), uri.clone()))
            })
            .collect::<Vec<_>>();
        FrameData::from_ratatui_buffer_with_hyperlinks(buffer, cursor, &hyperlinks)
    }

    fn sample_frame() -> FrameData {
        let shapes = [
            shepr_vt::UnderlineStyle::None,
            shepr_vt::UnderlineStyle::Single,
            shepr_vt::UnderlineStyle::Double,
            shepr_vt::UnderlineStyle::Curly,
            shepr_vt::UnderlineStyle::Dotted,
            shepr_vt::UnderlineStyle::Dashed,
        ];
        let width = 6u16;
        let height = 3u16;
        let mut cells = Vec::new();
        for index in 0..usize::from(width) * usize::from(height) {
            cells.push(CellData {
                symbol: ((b'a' + u8::try_from(index).unwrap_or(0)) as char).to_string(),
                fg: WireColor::Indexed(u8::try_from(index).unwrap_or(0)),
                bg: WireColor::Reset,
                style: WireStyle {
                    flags: WireStyleFlags::default(),
                    underline: shapes[index % shapes.len()],
                },
                skip: index == 4,
                // Link 1 is used first by a later cell than link 0 on purpose: the
                // frame's URI order is not first-use order.
                hyperlink: match index {
                    2 | 3 => Some(1),
                    7 | 8 => Some(0),
                    _ => None,
                },
            });
        }
        FrameData {
            cells,
            width,
            height,
            cursor: None,
            hyperlinks: vec!["https://a.example".into(), "https://b.example".into()],
        }
    }

    fn check(edit: impl FnOnce(&mut ratatui::buffer::Buffer)) {
        let frame = sample_frame();
        let Some(mut buffer) = frame.to_ratatui_buffer() else {
            panic!("frame converts to a buffer");
        };
        edit(&mut buffer);
        let expected = full_rebuild(&frame, &buffer, None);
        let mut actual = frame.clone();
        actual.replace_from_ratatui_buffer_preserving_effects(&buffer, None);
        assert_eq!(actual, expected);
    }

    #[test]
    fn untouched_buffer_matches_full_rebuild() {
        check(|_| {});
    }

    #[test]
    fn restyled_linked_cell_keeps_link_and_underline_shapes() {
        check(|buffer| {
            if let Some(cell) = buffer.cell_mut((2, 0)) {
                cell.set_style(Style::default().bg(Color::Red).add_modifier(Modifier::BOLD));
            }
            if let Some(cell) = buffer.cell_mut((1, 1)) {
                cell.set_style(Style::default().fg(Color::Green));
            }
        });
    }

    #[test]
    fn overwritten_linked_cells_drop_links_and_reindex() {
        // Overwriting both cells of link 1 leaves only link 0, which must become index 0.
        check(|buffer| {
            for x in [2u16, 3] {
                if let Some(cell) = buffer.cell_mut((x, 0)) {
                    cell.set_symbol("#");
                }
            }
        });
        // Overwriting one of link 0's cells keeps it referenced.
        check(|buffer| {
            if let Some(cell) = buffer.cell_mut((1, 1)) {
                cell.set_symbol("#");
            }
        });
        // Overwriting every linked cell leaves no hyperlinks.
        check(|buffer| {
            for (x, y) in [(2u16, 0u16), (3, 0), (1, 1), (2, 1)] {
                if let Some(cell) = buffer.cell_mut((x, y)) {
                    cell.set_symbol("#");
                }
            }
        });
    }

    #[test]
    fn cursor_and_mismatched_buffer_match_full_rebuild() {
        let frame = sample_frame();
        let cursor = Some(CursorState {
            x: 1,
            y: 1,
            visible: true,
            shape: crate::CursorShapeParam::Default,
        });
        let smaller = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, 4, 2));
        let expected = full_rebuild(&frame, &smaller, cursor.clone());
        let mut actual = frame.clone();
        actual.replace_from_ratatui_buffer_preserving_effects(&smaller, cursor);
        assert_eq!(actual, expected);
    }
}
