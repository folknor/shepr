//! Sparse changed-cell planning for typed surface updates.

use serde::Serialize;

use super::{
    CellData, MAX_SURFACE_HYPERLINKS, MAX_SURFACE_PANES, MAX_SURFACE_PATCH_SPANS,
    MAX_SURFACE_SPLIT_PATH, MAX_SURFACE_SPLITS, PaneSurfaceFrame, PaneSurfacePatchRow,
    ServerMessage,
};

#[derive(Debug)]
pub enum SurfaceDeltaError {
    InvalidGrid,
    InvalidRows(&'static str),
    SpanOutOfBounds,
    Encoding(super::codec::CodecError),
}

impl std::fmt::Display for SurfaceDeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGrid => f.write_str("cell grid does not match its dimensions"),
            Self::InvalidRows(reason) => f.write_str(reason),
            Self::SpanOutOfBounds => f.write_str("patch span exceeds the cell grid"),
            Self::Encoding(error) => write!(f, "{error}"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for SurfaceDeltaError {}

/// Whether a full surface can safely be represented as delta metadata.
fn metadata_fits(surface: &PaneSurfaceFrame) -> bool {
    surface.frame.grid().is_ok()
        && surface.frame.hyperlinks.len() <= MAX_SURFACE_HYPERLINKS
        && surface.panes.len() <= MAX_SURFACE_PANES
        && surface.splits.len() <= MAX_SURFACE_SPLITS
        && surface
            .splits
            .iter()
            .all(|split| split.path.len() <= MAX_SURFACE_SPLIT_PATH)
}

fn unchanged_plan(
    last: &PaneSurfaceFrame,
    surface: &PaneSurfaceFrame,
    baseline: &super::surface_reuse::Baseline<'_>,
) -> Option<SurfaceDeltaPlan> {
    if !baseline.accepts_surface(surface)
        || surface.projection_revision != last.projection_revision
        || !surface.topology().same_topology(&last.topology())
        || surface.frame != last.frame
        || surface.panes != last.panes
    {
        return None;
    }
    let update = baseline.update(surface, Vec::new(), last);
    Some(SurfaceDeltaPlan::Unchanged(ServerMessage::SurfaceUpdate(
        update,
    )))
}

fn unchanged_message(
    last: &PaneSurfaceFrame,
    surface: &PaneSurfaceFrame,
    baseline: &super::surface_reuse::Baseline<'_>,
) -> SurfaceDeltaPlan {
    // This path and unchanged_plan both require equal projection metadata at
    // the same projection revision, so Baseline::update emits Patch metadata
    // with only the cursor and an empty pane list, never the candidate's
    // hyperlink or split tables. That is why neither checks metadata_fits: a
    // live baseline passed the bounded wire serializers before it was
    // committed, and the client rejects an invalid grid before retaining or
    // applying an update.
    let update = baseline.update(surface, Vec::new(), last);
    SurfaceDeltaPlan::Unchanged(ServerMessage::SurfaceUpdate(update))
}

fn projection_metadata_is_unchanged(last: &PaneSurfaceFrame, surface: &PaneSurfaceFrame) -> bool {
    surface.projection_revision == last.projection_revision
        && surface.topology().same_topology(&last.topology())
        && surface.frame.cursor == last.frame.cursor
        && surface.panes == last.panes
}

/// Copies each span into a row-major grid of `width` x `height` cells.
///
/// Every span is checked against the shared span rule before any cell is
/// written, so a rejected set leaves the grid untouched.
pub(crate) fn apply_rows(
    cells: &mut [CellData],
    width: u16,
    height: u16,
    rows: &[PaneSurfacePatchRow],
) -> Result<(), SurfaceDeltaError> {
    super::FrameGrid::new(cells, width, height).map_err(|_| SurfaceDeltaError::InvalidGrid)?;
    crate::validate_patch_rows(width, height, rows).map_err(SurfaceDeltaError::InvalidRows)?;
    for row in rows {
        let start = usize::from(row.y) * usize::from(width) + usize::from(row.x);
        let end = start + row.cells.len();
        cells
            .get_mut(start..end)
            .ok_or(SurfaceDeltaError::SpanOutOfBounds)?
            .clone_from_slice(&row.cells);
    }
    Ok(())
}

struct CellSpan<'a> {
    x: u16,
    y: u16,
    cells: &'a [CellData],
}

fn changed_rows<'a>(
    last: &[CellData],
    next: &'a [CellData],
    width: u16,
) -> Option<Vec<CellSpan<'a>>> {
    let mut rows = Vec::new();
    if width == 0 {
        return Some(rows);
    }
    let mut changed_cells = 0;
    for (y, (old_row, new_row)) in last
        .chunks(usize::from(width))
        .zip(next.chunks(usize::from(width)))
        .enumerate()
    {
        let mut x = 0;
        while x < new_row.len() {
            if old_row[x] == new_row[x] {
                x += 1;
                continue;
            }
            let start = x;
            while x < new_row.len() && old_row[x] != new_row[x] {
                x += 1;
            }
            let span = CellSpan {
                // `start`/`y` are bounded by `width` (a u16) via chunks(usize::from(width)).
                x: u16::try_from(start).unwrap_or(u16::MAX),
                y: u16::try_from(y).unwrap_or(u16::MAX),
                cells: &new_row[start..x],
            };
            changed_cells += span.cells.len();
            // Dense updates are cheaper to send whole. This is a planning
            // heuristic, not an exact encoded-size comparison.
            if rows.len() == MAX_SURFACE_PATCH_SPANS || changed_cells > next.len() / 2 {
                return None;
            }
            rows.push(span);
        }
    }
    Some(rows)
}

fn encoded_size(value: &impl Serialize) -> Result<usize, SurfaceDeltaError> {
    super::codec::encoded_len(value).map_err(SurfaceDeltaError::Encoding)
}

/// Result of comparing a full candidate with its committed surface baseline.
pub enum SurfaceDeltaPlan {
    /// The candidate cannot be represented more cheaply as an update.
    Full,
    /// The content is unchanged. Keep the update only when a refresh is forced.
    Unchanged(ServerMessage),
    /// Send this update and retain the candidate as the new baseline.
    Compact(ServerMessage),
}

pub fn message(
    last: &PaneSurfaceFrame,
    surface: &PaneSurfaceFrame,
) -> Result<SurfaceDeltaPlan, SurfaceDeltaError> {
    let baseline = super::surface_reuse::Baseline::new(
        &last.boot_id,
        last.projection_revision,
        last.surface_revision,
    );
    let Some(expected_cells) = super::surface_grid_size(surface.frame.width, surface.frame.height)
    else {
        return Ok(unchanged_plan(last, surface, &baseline).unwrap_or(SurfaceDeltaPlan::Full));
    };
    if !baseline.accepts_surface(surface)
        || last.frame.width != surface.frame.width
        || last.frame.height != surface.frame.height
        || last.frame.cells.len() != expected_cells
        || surface.frame.cells.len() != expected_cells
    {
        return Ok(unchanged_plan(last, surface, &baseline).unwrap_or(SurfaceDeltaPlan::Full));
    }
    // Every cell has a string length prefix, a grid-width discriminant, two
    // color discriminants, a skip byte and a hyperlink option tag: at least six
    // bytes, even ignoring its symbol and style. This lower bound avoids another
    // full-grid serialization pass on this per-client path while guaranteeing
    // any chosen cell delta is smaller. It may miss useful deltas on small grids
    // or when most of the full message consists of metadata.
    let full_size = expected_cells.saturating_mul(6);
    let Some(rows) = changed_rows(&last.frame.cells, &surface.frame.cells, surface.frame.width)
    else {
        return Ok(SurfaceDeltaPlan::Full);
    };
    // The cell scan already established an unchanged grid. Check the compact
    // metadata conditions before cloning any projection metadata into an update.
    if rows.is_empty() && projection_metadata_is_unchanged(last, surface) {
        return Ok(unchanged_message(last, surface, &baseline));
    }
    if !metadata_fits(surface) {
        return Ok(SurfaceDeltaPlan::Full);
    }
    let spans = rows
        .into_iter()
        .map(|row| PaneSurfacePatchRow {
            x: row.x,
            y: row.y,
            cells: row.cells.to_vec(),
        })
        .collect();
    let update = baseline.update(surface, spans, last);
    // Metadata-only updates always retain the grid. Counting potentially large
    // projection metadata cannot improve this choice.
    let metadata_only = update.spans.is_empty();
    let message = ServerMessage::SurfaceUpdate(update);
    if metadata_only {
        return Ok(SurfaceDeltaPlan::Compact(message));
    }
    let size = encoded_size(&message)?;
    if size < full_size {
        Ok(SurfaceDeltaPlan::Compact(message))
    } else {
        Ok(SurfaceDeltaPlan::Full)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn surface() -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: "1-1".into(),
            projection_revision: super::super::ProjectionRevision::new(1),
            surface_revision: super::super::SurfaceRevision::new(1),
            frame: super::super::FrameData::blank(200, 100),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn truncated_candidate_cannot_be_reported_as_an_unchanged_grid() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);
        next.frame.cells.pop();
        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Full
        ));
    }

    #[test]
    fn sparse_candidate_is_smaller_and_keeps_large_hyperlink_metadata_off_wire() {
        let mut last = surface();
        last.frame.hyperlinks = vec!["https://example.test/".repeat(4096)];
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);
        next.frame.cells[0].symbol = "z".into();
        let delta = match message(&last, &next).expect("planning") {
            SurfaceDeltaPlan::Compact(delta) => delta,
            _ => panic!("expected compact delta"),
        };
        let full = ServerMessage::PaneSurface(next.clone());
        assert!(encoded_size(&delta).expect("size") < encoded_size(&full).expect("size"));
        assert!(encoded_size(&delta).expect("size") < 1024);
        let mut decoder = super::super::surface_reuse::Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(last))
            .expect("baseline");
        decoder.decode(delta).expect("update");
        assert_eq!(decoder.current_surface(), Some(next));
    }

    #[test]
    fn dense_changes_use_the_full_surface() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);
        for cell in &mut next.frame.cells {
            cell.symbol = "z".into();
        }
        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Full
        ));
    }

    #[test]
    fn unchanged_surface_is_reported_after_the_cell_scan() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);
        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Unchanged(ServerMessage::SurfaceUpdate(_))
        ));
    }

    #[test]
    fn unchanged_surface_with_oversized_metadata_is_reported_before_full_fallback() {
        let mut last = surface();
        last.frame.hyperlinks = vec![String::new(); MAX_SURFACE_HYPERLINKS + 1];
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);

        assert_unchanged_update_is_compact(message(&last, &next).expect("planning"));
    }

    #[test]
    fn changed_surface_with_oversized_metadata_still_uses_the_full_surface() {
        let mut last = surface();
        last.frame.hyperlinks = vec![String::new(); MAX_SURFACE_HYPERLINKS + 1];
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);
        next.frame.cells[0].symbol = "x".into();

        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Full
        ));
    }

    #[test]
    fn unchanged_surface_with_invalid_grid_is_reported_before_full_fallback() {
        let mut last = surface();
        last.frame.width = u16::MAX;
        last.frame.height = 1;
        last.frame.cells.clear();
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);

        assert_unchanged_update_is_compact(message(&last, &next).expect("planning"));
    }

    fn assert_unchanged_update_is_compact(plan: SurfaceDeltaPlan) {
        let SurfaceDeltaPlan::Unchanged(ServerMessage::SurfaceUpdate(update)) = plan else {
            panic!("expected unchanged surface update");
        };
        assert!(update.spans.is_empty());
        assert!(matches!(
            &update.meta,
            Some(super::super::SurfaceMeta::Patch(meta)) if meta.panes.is_empty()
        ));
        let mut bytes = Vec::new();
        super::super::write_message(&mut bytes, &ServerMessage::SurfaceUpdate(update))
            .expect("compact unchanged update encodes");
    }
}
