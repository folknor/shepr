use shepr_core::geometry::PanePixelExtent;

/// Whole-window pixel extent for mouse mapping. The terminal can include
/// padding, so this cannot be reconstructed from the reported cell pitch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPixelExtent {
    grid: shepr_core::geometry::GridSize,
    width_px: u32,
    height_px: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostPixels {
    pub x: u32,
    pub y: u32,
    pub geometry: HostPixelExtent,
}

impl HostPixelExtent {
    pub fn width_px(self) -> u32 {
        self.width_px
    }

    pub fn height_px(self) -> u32 {
        self.height_px
    }

    pub fn grid(self) -> shepr_core::geometry::GridSize {
        self.grid
    }

    pub fn cols(self) -> u16 {
        self.grid.cols.get()
    }

    pub fn rows(self) -> u16 {
        self.grid.rows.get()
    }

    pub fn new(cols: u16, rows: u16, width_px: u32, height_px: u32) -> Option<Self> {
        (width_px > 0 && height_px > 0).then_some(Self {
            grid: shepr_core::geometry::GridSize::new(cols, rows)?,
            width_px,
            height_px,
        })
    }

    pub fn current() -> Option<Self> {
        let size = crossterm::terminal::window_size().ok()?;
        Self::new(
            size.columns,
            size.rows,
            u32::from(size.width),
            u32::from(size.height),
        )
    }

    pub fn cell(self, x: u32, y: u32) -> Option<(u16, u16)> {
        let cols = self.grid.cols.get();
        let rows = self.grid.rows.get();
        let x = x.checked_sub(1)?;
        let y = y.checked_sub(1)?;
        if x >= self.width_px || y >= self.height_px {
            return None;
        }
        let width_px = grid_extent(cols, self.width_px)?;
        let height_px = grid_extent(rows, self.height_px)?;
        Some((
            grid_cell(x.min(width_px - 1), cols, width_px)?,
            grid_cell(y.min(height_px - 1), rows, height_px)?,
        ))
    }

    /// Maps a mouse report, keeping a drag or release inside the nearest edge
    /// cell when a terminal reports coordinates beyond its ioctl pixel extent.
    pub fn cell_for_event(
        self,
        x: u32,
        y: u32,
        kind: crossterm::event::MouseEventKind,
    ) -> Option<(u16, u16)> {
        if let Some(cell) = self.cell(x, y) {
            return Some(cell);
        }
        if !matches!(
            kind,
            crossterm::event::MouseEventKind::Drag(_) | crossterm::event::MouseEventKind::Up(_)
        ) {
            return None;
        }
        self.cell(x.clamp(1, self.width_px), y.clamp(1, self.height_px))
    }
}

impl HostPixels {
    /// Maps this host pixel into `extent`, the pixel extent the pane's child
    /// was told. `None` when the pixel is outside `inner` or the host window.
    pub fn pane_position(
        self,
        inner: ratatui::layout::Rect,
        extent: PanePixelExtent,
    ) -> Option<(u32, u32)> {
        let child_width_px = u32::from(extent.width().get());
        let child_height_px = u32::from(extent.height().get());
        let (host_column, host_row) = self.geometry.cell(self.x, self.y)?;
        let end_column = inner.x.checked_add(inner.width)?;
        let end_row = inner.y.checked_add(inner.height)?;
        if host_column < inner.x
            || host_column >= end_column
            || host_row < inner.y
            || host_row >= end_row
        {
            return None;
        }
        Some((
            map_axis_within_cell(
                self.x,
                host_column,
                inner.x,
                inner.width,
                self.geometry.grid.cols.get(),
                self.geometry.width_px,
                child_width_px,
            )?,
            map_axis_within_cell(
                self.y,
                host_row,
                inner.y,
                inner.height,
                self.geometry.grid.rows.get(),
                self.geometry.height_px,
                child_height_px,
            )?,
        ))
    }
}

fn map_axis_within_cell(
    pixel: u32,
    host_cell: u16,
    pane_start: u16,
    pane_cells: u16,
    host_cells: u16,
    host_extent: u32,
    child_extent: u32,
) -> Option<u32> {
    let local_cell = host_cell.checked_sub(pane_start)?;
    if local_cell >= pane_cells {
        return None;
    }
    let host_grid_extent = grid_extent(host_cells, host_extent)?;
    let source_start = boundary(host_cell, host_cells, host_grid_extent)?;
    let source_end = boundary(host_cell.checked_add(1)?, host_cells, host_grid_extent)?;
    let target_start = boundary(local_cell, pane_cells, child_extent)?;
    let target_end = boundary(local_cell.checked_add(1)?, pane_cells, child_extent)?;
    let source_width = source_end.checked_sub(source_start)?;
    let target_width = target_end.checked_sub(target_start)?;
    let offset = pixel.checked_sub(1)?.checked_sub(source_start)?;
    if source_width == 0 || target_width == 0 || offset >= source_width {
        return None;
    }
    target_start
        .checked_add(scale(offset, source_width, target_width))?
        .checked_add(1)
}

fn boundary(index: u16, count: u16, extent: u32) -> Option<u32> {
    (count > 0 && index <= count && extent > 0).then(|| {
        let value = u64::from(index) * u64::from(extent) / u64::from(count);
        // value <= extent (a u32) because index <= count, so this never truncates.
        u32::try_from(value).unwrap_or(extent)
    })
}

fn grid_extent(count: u16, extent: u32) -> Option<u32> {
    let count = u32::from(count);
    (count > 0 && extent > 0).then(|| (extent / count).max(1) * count)
}

fn grid_cell(pixel: u32, count: u16, extent: u32) -> Option<u16> {
    if count == 0 || extent == 0 || pixel >= extent {
        return None;
    }
    let cell = ((u64::from(pixel) + 1) * u64::from(count) - 1) / u64::from(extent);
    u16::try_from(cell).ok().filter(|cell| *cell < count)
}

fn scale(pixel: u32, source: u32, target: u32) -> u32 {
    let cap = target.saturating_sub(1);
    let value = ((u64::from(pixel) * u64::from(target)) / u64::from(source)).min(u64::from(cap));
    // value <= cap (a u32) by construction, so this never truncates.
    u32::try_from(value).unwrap_or(cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn extent(cols: u16, rows: u16, width: u16, height: u16) -> PanePixelExtent {
        PanePixelExtent::new(
            shepr_core::geometry::GridSize::new(cols, rows).expect("test grid"),
            width,
            height,
        )
        .expect("test extent")
    }

    #[test]
    fn integer_cell_pitch_ignores_trailing_pixel_remainder() {
        let geometry = HostPixelExtent::new(127, 31, 1_276, 626).expect("test precondition");
        assert_eq!(geometry.cell(220, 1), Some((21, 0)));
        assert_eq!(geometry.cell(221, 61), Some((22, 3)));
        let right_pane = ratatui::layout::Rect::new(22, 3, 105, 20);
        assert_eq!(
            HostPixels {
                x: 221,
                y: 61,
                geometry,
            }
            .pane_position(right_pane, extent(105, 20, 1_050, 400)),
            Some((1, 1))
        );
        assert_eq!(geometry.cell(1_270, 620), Some((126, 30)));
        assert_eq!(
            HostPixels {
                x: 1_270,
                y: 460,
                geometry,
            }
            .pane_position(right_pane, extent(105, 20, 1_050, 400)),
            Some((1_050, 400))
        );

        let full_grid = ratatui::layout::Rect::new(0, 0, 127, 31);
        for x in 1_271..=1_276 {
            assert_eq!(geometry.cell(x, 1), Some((126, 0)));
            assert_eq!(
                HostPixels { x, y: 1, geometry }
                    .pane_position(full_grid, extent(127, 31, 1_270, 620)),
                None
            );
        }
        for y in 621..=626 {
            assert_eq!(geometry.cell(1, y), Some((0, 30)));
            assert_eq!(
                HostPixels { x: 1, y, geometry }
                    .pane_position(full_grid, extent(127, 31, 1_270, 620)),
                None
            );
        }
        assert_eq!(geometry.cell(1_277, 1), None);
        assert_eq!(geometry.cell(1, 627), None);
    }

    #[test]
    fn geometry_rejects_outside_pixels_and_maps_cells() {
        let geometry = HostPixelExtent::new(80, 24, 800, 480).expect("test precondition");
        assert_eq!(geometry.cell(1, 1), Some((0, 0)));
        assert_eq!(geometry.cell(800, 480), Some((79, 23)));
        assert_eq!(geometry.cell(801, 1), None);
        assert_eq!(geometry.cell(0, 1), None);
    }

    #[test]
    fn out_of_extent_mouse_drags_and_releases_clamp_to_edge_cells() {
        let geometry = HostPixelExtent::new(80, 24, 800, 480).expect("test precondition");
        use crossterm::event::{MouseButton, MouseEventKind};

        assert_eq!(
            geometry.cell_for_event(801, 481, MouseEventKind::Up(MouseButton::Left)),
            Some((79, 23))
        );
        assert_eq!(
            geometry.cell_for_event(0, 0, MouseEventKind::Drag(MouseButton::Left)),
            Some((0, 0))
        );
        assert_eq!(
            geometry.cell_for_event(801, 481, MouseEventKind::Down(MouseButton::Left)),
            None
        );
    }
}
