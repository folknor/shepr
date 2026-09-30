use super::*;
use serde::{Deserialize, Serialize};
use shepr_core::geometry::SplitBranch;

/// Origin-relative geometry for one pane in a rendered pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePane {
    pub pane_id: PublicPaneId,
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
    pub history_origin: shepr_vt::AbsRow,
}

/// One draggable BSP split handle relative to a pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceSplit {
    pub direction: PaneSurfaceSplitDirection,
    pub pos: u16,
    pub area: SurfaceRect,
    pub hit_rect: SurfaceRect,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_SPLIT_PATH, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_SPLIT_PATH, _, _>"
    )]
    pub path: Vec<SplitBranch>,
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

impl From<shepr_core::geometry::Rect> for SurfaceRect {
    fn from(rect: shepr_core::geometry::Rect) -> Self {
        Self {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

impl From<shepr_core::layout::Direction> for PaneSurfaceSplitDirection {
    fn from(direction: shepr_core::layout::Direction) -> Self {
        match direction {
            shepr_core::layout::Direction::Horizontal => Self::Horizontal,
            shepr_core::layout::Direction::Vertical => Self::Vertical,
        }
    }
}

/// One server-rendered workspace surface without sidebar or overlays.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfaceFrame {
    /// Endpoint process identity that produced this surface.
    pub boot_id: BootId,
    /// Projection revision whose focused IDs and topology produced this surface.
    pub projection_revision: ProjectionRevision,
    /// Monotonic revision for full surfaces and incremental patches on one connection.
    pub surface_revision: SurfaceRevision,
    pub frame: FrameData,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_PANES, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_PANES, _, _>"
    )]
    pub panes: Vec<PaneSurfacePane>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_SPLITS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_SPLITS, _, _>"
    )]
    pub splits: Vec<PaneSurfaceSplit>,
}

/// Projection and frame metadata for an incremental surface update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceMeta {
    pub frame: SurfaceFrameMeta,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_PANES, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_PANES, _, _>"
    )]
    pub panes: Vec<PaneSurfacePane>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_SPLITS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_SPLITS, _, _>"
    )]
    pub(crate) splits: Vec<PaneSurfaceSplit>,
}

/// The non-cell fields shared by a full frame and update metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceFrameMeta {
    pub(crate) width: u16,
    pub(crate) height: u16,
    pub cursor: Option<CursorState>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_HYPERLINKS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_HYPERLINKS, _, _>"
    )]
    pub(crate) hyperlinks: Vec<String>,
}

impl From<&PaneSurfaceFrame> for SurfaceMeta {
    fn from(surface: &PaneSurfaceFrame) -> Self {
        Self {
            frame: SurfaceFrameMeta {
                width: surface.frame.width,
                height: surface.frame.height,
                cursor: surface.frame.cursor.clone(),
                hyperlinks: surface.frame.hyperlinks.clone(),
            },
            panes: surface.panes.clone(),
            splits: surface.splits.clone(),
        }
    }
}

impl SurfaceMeta {
    pub(crate) fn into_surface(
        self,
        boot_id: BootId,
        projection_revision: ProjectionRevision,
        surface_revision: SurfaceRevision,
        cells: Vec<CellData>,
    ) -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id,
            projection_revision,
            surface_revision,
            frame: FrameData {
                cells,
                width: self.frame.width,
                height: self.frame.height,
                cursor: self.frame.cursor,
                hyperlinks: self.frame.hyperlinks,
            },
            panes: self.panes,
            splits: self.splits,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePatchRow {
    /// Origin-relative surface column where this changed cell span starts.
    pub x: u16,
    /// Origin-relative surface row.
    pub y: u16,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<{ MAX_SURFACE_DIMENSION as usize }, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<{ MAX_SURFACE_DIMENSION as usize }, _, _>"
    )]
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
pub fn validate_patch_rows(
    width: u16,
    height: u16,
    rows: &[PaneSurfacePatchRow],
) -> Result<(), &'static str> {
    let mut check = PatchSpanCheck::new(width, height);
    rows.iter()
        .try_for_each(|row| check.push(row.x, row.y, row.cells.len()))
}

/// Puts rows into the row-major order [`PatchSpanCheck`] requires.
pub fn sort_patch_rows(rows: &mut [PaneSurfacePatchRow]) {
    rows.sort_unstable_by_key(|row| (row.y, row.x));
}

/// Incremental terminal-cell update against one committed complete pane surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneSurfacePatch {
    pub boot_id: BootId,
    pub projection_revision: ProjectionRevision,
    pub base_surface_revision: SurfaceRevision,
    pub surface_revision: SurfaceRevision,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_PATCH_SPANS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_PATCH_SPANS, _, _>"
    )]
    pub rows: Vec<PaneSurfacePatchRow>,
    /// Updated metadata for panes whose terminal content changed.
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_PANES, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_PANES, _, _>"
    )]
    pub panes: Vec<PaneSurfacePane>,
    /// Final cursor relative to the pane surface.
    pub cursor: Option<CursorState>,
}

/// One incremental surface update. An absent metadata value retains the
/// previous projection; an empty span list retains every terminal cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceUpdate {
    pub boot_id: BootId,
    pub base_surface_revision: SurfaceRevision,
    pub surface_revision: SurfaceRevision,
    pub base_projection_revision: ProjectionRevision,
    pub projection_revision: ProjectionRevision,
    pub meta: Option<SurfaceMeta>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_PATCH_SPANS, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_PATCH_SPANS, _, _>"
    )]
    pub spans: Vec<PaneSurfacePatchRow>,
}
