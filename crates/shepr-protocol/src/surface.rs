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

pub use shepr_vt::ScrollMetrics as PaneSurfaceScrollMetrics;

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

/// The fields a cell patch retains. Cursor and per-pane observations may change;
/// pane order, dimensions, hyperlinks and split handles must remain stable.
/// This is a borrowed view so checking topology never clones hyperlink tables.
pub struct SurfaceTopology<'a> {
    width: u16,
    height: u16,
    hyperlinks: &'a [String],
    panes: &'a [PaneSurfacePane],
    splits: &'a [PaneSurfaceSplit],
}

impl SurfaceTopology<'_> {
    pub fn same_topology(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.hyperlinks == other.hyperlinks
            && self.splits == other.splits
            && self.panes.len() == other.panes.len()
            && self
                .panes
                .iter()
                .zip(other.panes)
                .all(|(left, right)| left.pane_id == right.pane_id)
    }
}

impl PaneSurfaceFrame {
    /// Whether this surface has the pane grid requested by the client.
    pub fn is_sized_for(&self, size: ClientSurfaceSize) -> bool {
        self.frame.width == size.cols && self.frame.height == size.rows
    }

    pub fn topology(&self) -> SurfaceTopology<'_> {
        SurfaceTopology {
            width: self.frame.width,
            height: self.frame.height,
            hyperlinks: &self.frame.hyperlinks,
            panes: &self.panes,
            splits: &self.splits,
        }
    }
}

/// Metadata either replaces the projection or changes only cursor and named panes.
/// Patch metadata retains dimensions, split topology and hyperlink indices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SurfaceMeta {
    Projection(SurfaceProjectionMeta),
    Patch(SurfacePatchMeta),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfacePatchMeta {
    pub cursor: Option<CursorState>,
    #[serde(
        serialize_with = "codec::serialize_bounded_vec::<MAX_SURFACE_PANES, _, _>",
        deserialize_with = "codec::deserialize_bounded_vec::<MAX_SURFACE_PANES, _, _>"
    )]
    pub panes: Vec<PaneSurfacePane>,
}

impl From<&PaneSurfaceFrame> for SurfaceMeta {
    fn from(surface: &PaneSurfaceFrame) -> Self {
        Self::Projection(surface.into())
    }
}

/// Projection and frame metadata for an incremental surface update.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceProjectionMeta {
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

impl From<&PaneSurfaceFrame> for SurfaceProjectionMeta {
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

impl SurfaceProjectionMeta {
    pub fn topology(&self) -> SurfaceTopology<'_> {
        SurfaceTopology {
            width: self.frame.width,
            height: self.frame.height,
            hyperlinks: &self.frame.hyperlinks,
            panes: &self.panes,
            splits: &self.splits,
        }
    }

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

/// Client-local incremental update decoded from a wire `SurfaceUpdate` against
/// one committed complete pane surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneSurfacePatch {
    pub boot_id: BootId,
    pub projection_revision: ProjectionRevision,
    pub base_surface_revision: SurfaceRevision,
    pub surface_revision: SurfaceRevision,
    pub rows: Vec<PaneSurfacePatchRow>,
    /// Updated metadata for panes whose terminal content changed.
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

#[cfg(test)]
mod scroll_metrics_tests {
    use super::PaneSurfaceScrollMetrics;
    use crate::codec;

    #[test]
    fn scroll_metrics_wire_rejects_an_offset_outside_history() {
        let fields = shepr_vt::ScrollMetricsFields {
            offset_from_bottom: 11,
            max_offset_from_bottom: 10,
            viewport_rows: 3,
            history_origin: shepr_vt::AbsRow(40),
        };
        let mut bytes = Vec::new();
        codec::encode_into(&mut bytes, &fields).expect("encode metric fields");
        assert!(codec::from_slice_exact::<PaneSurfaceScrollMetrics>(&bytes).is_err());
    }

    #[test]
    fn scroll_metrics_wire_preserves_the_history_base() {
        let metrics = PaneSurfaceScrollMetrics::new(4, 10, 3, shepr_vt::AbsRow(40));
        let mut bytes = Vec::new();
        codec::encode_into(&mut bytes, &metrics).expect("encode metrics");
        // Exact decoding also checks that every field was consumed.
        let decoded: PaneSurfaceScrollMetrics =
            codec::from_slice_exact(&bytes).expect("decode metrics");
        assert_eq!(decoded, metrics);
        assert_eq!(decoded.viewport_top_row(), shepr_vt::AbsRow(46));
    }
}
