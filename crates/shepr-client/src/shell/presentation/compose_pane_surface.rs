use super::composition::wire_cells::{blank, split_glyph_cells};
use super::*;

/// Copies `source` cells into `target` at `area`, clipped to both, keeping each cell's
/// underline shape and remapping hyperlinks into `target`'s table. It shares only the
/// glyph repair with `wire_cells::overwrite`: a target glyph split by the pasted region
/// loses its uncovered part, and a source glyph cut by the clip becomes a blank.
pub(super) fn compose_pane_surface(target: &mut FrameData, source: &FrameData, area: Rect) {
    let target_width = usize::from(target.width);
    let source_width = usize::from(source.width);
    let copy_width = source
        .width
        .min(area.width)
        .min(target.width.saturating_sub(area.x));
    let copy_height = source
        .height
        .min(area.height)
        .min(target.height.saturating_sub(area.y));
    let hyperlink_base = u32::try_from(target.hyperlinks.len()).unwrap_or(u32::MAX);
    target.hyperlinks.extend(source.hyperlinks.iter().cloned());
    let consistent = target.cells.len() == target_width * usize::from(target.height)
        && source.cells.len() == source_width * usize::from(source.height);

    if consistent && copy_width > 0 {
        let mut target_covered = vec![false; target_width];
        target_covered[usize::from(area.x)..usize::from(area.x + copy_width)].fill(true);
        let mut source_covered = vec![false; source_width];
        source_covered[..usize::from(copy_width)].fill(true);
        for row in 0..copy_height {
            let source_row = &source.cells[usize::from(row) * source_width..][..source_width];
            let target_start = usize::from(area.y + row) * target_width;
            let target_row = &mut target.cells[target_start..target_start + target_width];
            let target_view = &*target_row;
            let target_remnants = split_glyph_cells(
                target_width,
                move |x| target_view[x].symbol.as_str(),
                &target_covered,
                false,
            );
            for x in target_remnants {
                blank(&mut target_row[x]);
            }
            let source_remnants = if source_width > usize::from(copy_width) {
                split_glyph_cells(
                    source_width,
                    move |x| source_row[x].symbol.as_str(),
                    &source_covered,
                    true,
                )
            } else {
                Vec::new()
            };
            for (col, source_cell) in source_row[..usize::from(copy_width)].iter().enumerate() {
                let mut cell = source_cell.clone();
                cell.hyperlink = source_cell.hyperlink.and_then(|index| {
                    ((index as usize) < source.hyperlinks.len()).then_some(hyperlink_base + index)
                });
                if source_remnants.contains(&col) {
                    blank(&mut cell);
                }
                target_row[usize::from(area.x) + col] = cell;
            }
        }
    }

    target.cursor = source.cursor.as_ref().and_then(|cursor| {
        (cursor.x < copy_width && cursor.y < copy_height).then(|| shepr_protocol::CursorState {
            x: area.x + cursor.x,
            y: area.y + cursor.y,
            visible: cursor.visible,
            shape: cursor.shape,
        })
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{CellData, WireColor};
    use shepr_vt::UnderlineStyle;

    /// One row of cells, one per char; `~` is an empty-symbol wide tail as pane surfaces
    /// write them.
    fn frame(row: &str) -> FrameData {
        let cells = row
            .chars()
            .map(|c| CellData {
                symbol: if c == '~' {
                    String::new()
                } else {
                    c.to_string()
                },
                fg: WireColor::Reset,
                bg: WireColor::Reset,
                style: shepr_protocol::WireStyle::default(),
                skip: false,
                hyperlink: None,
            })
            .collect::<Vec<_>>();
        FrameData {
            width: u16::try_from(cells.len()).expect("test row fits"),
            height: 1,
            cells,
            cursor: None,
            hyperlinks: Vec::new(),
        }
    }

    fn text(frame: &FrameData) -> String {
        frame
            .cells
            .iter()
            .map(|cell| cell.symbol.as_str())
            .collect()
    }

    #[test]
    fn keeps_shapes_remaps_links_and_repairs_a_split_target_glyph() {
        let mut target = frame("|漢~|");
        target.hyperlinks = vec!["https://old.example".into()];
        target.cells[3].hyperlink = Some(0);
        let mut source = frame("XY");
        source.hyperlinks = vec!["https://new.example".into()];
        source.cells[0].hyperlink = Some(0);
        source.cells[1].style.underline = UnderlineStyle::Dotted;
        source.cells[1].hyperlink = Some(7);
        // Pasting over the tail only must not leave the target's lead as half a glyph.
        compose_pane_surface(&mut target, &source, Rect::new(2, 0, 1, 1));
        assert_eq!(text(&target), "| X|");
        assert_eq!(target.hyperlinks.len(), 2);
        assert_eq!(target.cells[2].hyperlink, Some(1));
        assert_eq!(target.cells[3].hyperlink, Some(0));

        let mut target = frame("....");
        compose_pane_surface(&mut target, &source, Rect::new(1, 0, 2, 1));
        assert_eq!(target.cells[2].style.underline, UnderlineStyle::Dotted);
        assert_eq!(target.cells[2].hyperlink, None, "dangling link is dropped");
    }

    #[test]
    fn blanks_a_source_glyph_cut_by_the_clip() {
        let mut target = frame("....");
        let source = frame("a漢~");
        compose_pane_surface(&mut target, &source, Rect::new(0, 0, 2, 1));
        assert_eq!(text(&target), "a ..");
    }

    #[test]
    fn clips_to_the_target_and_takes_the_cursor_from_the_visible_part() {
        let mut target = frame("....");
        let mut source = frame("abcd");
        source.cursor = Some(shepr_protocol::CursorState {
            x: 3,
            y: 0,
            visible: true,
            shape: shepr_protocol::CursorShapeParam::Default,
        });
        compose_pane_surface(&mut target, &source, Rect::new(3, 0, 4, 1));
        assert_eq!(text(&target), "...a");
        assert_eq!(target.cursor, None);
    }
}
