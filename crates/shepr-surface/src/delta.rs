//! Sparse changed-cell planning for typed surface updates: the server's choice
//! between sending a full surface and an update against a client's committed
//! baseline.

use shepr_protocol::{
    CellData, MAX_SURFACE_PANES, MAX_SURFACE_SPLIT_PATH, MAX_SURFACE_SPLITS, PaneSurfaceFrame,
    PaneSurfacePatchRow, ServerMessage,
};

use crate::decode::Baseline;
use crate::patch::PatchSpanCollector;

#[derive(Debug)]
pub enum SurfaceDeltaError {
    InvalidGrid,
    InvalidRows(&'static str),
    TooManySpans,
    SpanOutOfBounds,
    Encoding(shepr_protocol::codec::CodecError),
}

impl std::fmt::Display for SurfaceDeltaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGrid => f.write_str("cell grid does not match its dimensions"),
            Self::InvalidRows(reason) => f.write_str(reason),
            Self::TooManySpans => f.write_str("surface patch exceeds the span limit"),
            Self::SpanOutOfBounds => f.write_str("patch span exceeds the cell grid"),
            Self::Encoding(error) => write!(f, "{error}"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for SurfaceDeltaError {}

/// Whether a full surface can safely be represented as delta metadata. The
/// frame's own grid and link table are within budget by construction.
fn metadata_fits(surface: &PaneSurfaceFrame) -> bool {
    surface.panes.len() <= MAX_SURFACE_PANES
        && surface.splits.len() <= MAX_SURFACE_SPLITS
        && surface
            .splits
            .iter()
            .all(|split| split.path.len() <= MAX_SURFACE_SPLIT_PATH)
}

fn unchanged_plan(
    last: &PaneSurfaceFrame,
    surface: &PaneSurfaceFrame,
    baseline: &Baseline<'_>,
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
    baseline: &Baseline<'_>,
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
        && surface.frame.cursor() == last.frame.cursor()
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
    shepr_protocol::FrameGrid::new(cells, width, height)
        .map_err(|_| SurfaceDeltaError::InvalidGrid)?;
    crate::patch::validate_spans(width, height, rows).map_err(|error| match error {
        crate::patch::PatchSpanError::LimitExceeded => SurfaceDeltaError::TooManySpans,
        crate::patch::PatchSpanError::InvalidRows(reason) => SurfaceDeltaError::InvalidRows(reason),
    })?;
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
    let mut rows = PatchSpanCollector::new();
    if width == 0 {
        return Some(rows.into_vec());
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
            if rows.push(span).is_err() || changed_cells > next.len() / 2 {
                return None;
            }
        }
    }
    Some(rows.into_vec())
}

fn encoded_size(message: &ServerMessage) -> Result<usize, SurfaceDeltaError> {
    shepr_protocol::codec::encoded_len(message).map_err(SurfaceDeltaError::Encoding)
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
    let baseline = Baseline::new(
        &last.boot_id,
        last.projection_revision,
        last.surface_revision,
    );
    // Both frames hold exactly width * height cells by construction, so equal
    // dimensions mean equal cell counts.
    let expected_cells = surface.frame.cells().len();
    if !baseline.accepts_surface(surface)
        || last.frame.width() != surface.frame.width()
        || last.frame.height() != surface.frame.height()
    {
        return Ok(unchanged_plan(last, surface, &baseline).unwrap_or(SurfaceDeltaPlan::Full));
    }
    // A lower bound on the full message's size from the smallest encoded cell.
    // This avoids another full-grid serialization pass on this per-client path
    // while guaranteeing any chosen cell delta is smaller. It may miss useful
    // deltas on small grids or when most of the full message consists of
    // metadata.
    let full_size = expected_cells.saturating_mul(crate::limits::MIN_ENCODED_CELL_BYTES);
    let Some(rows) = changed_rows(
        last.frame.cells(),
        surface.frame.cells(),
        surface.frame.width(),
    ) else {
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
    match update.meta.as_ref() {
        Some(shepr_protocol::SurfaceMeta::Projection(_)) => {
            if crate::patch::validate_spans(
                surface.frame.width(),
                surface.frame.height(),
                &update.spans,
            )
            .is_err()
            {
                return Ok(SurfaceDeltaPlan::Full);
            }
        }
        Some(shepr_protocol::SurfaceMeta::Patch(_)) | None => {
            if crate::decode::SurfaceBaseline::new(last)
                .admits_update(&update)
                .is_err()
            {
                return Ok(SurfaceDeltaPlan::Full);
            }
        }
    }
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
            boot_id: "1-1".parse().expect("canonical test boot id"),
            projection_revision: crate::test_counters::projection(1),
            surface_revision: crate::test_counters::surface(1),
            frame: shepr_protocol::FrameData::blank(200, 100).expect("test frame"),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn a_candidate_of_another_size_is_sent_in_full() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision = crate::test_counters::surface(2);
        next.frame = shepr_protocol::FrameData::blank(199, 100).expect("test frame");
        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Full
        ));
    }

    #[test]
    fn sparse_candidate_is_smaller_and_keeps_large_hyperlink_metadata_off_wire() {
        let mut last = surface();
        last.frame
            .set_hyperlinks(vec!["https://example.test/".repeat(4096)])
            .expect("no cell links");
        let mut next = last.clone();
        next.surface_revision = crate::test_counters::surface(2);
        next.frame.cells_mut()[0].symbol = "z".into();
        let delta = match message(&last, &next).expect("planning") {
            SurfaceDeltaPlan::Compact(delta) => delta,
            _ => panic!("expected compact delta"),
        };
        let full = ServerMessage::PaneSurface(next.clone());
        assert!(encoded_size(&delta).expect("size") < encoded_size(&full).expect("size"));
        assert!(encoded_size(&delta).expect("size") < 1024);
        let mut decoder = crate::decode::Decoder::default();
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
        next.surface_revision = crate::test_counters::surface(2);
        for cell in next.frame.cells_mut() {
            cell.symbol = "z".into();
        }
        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Full
        ));
    }

    #[test]
    fn too_many_fragmented_spans_use_the_full_surface() {
        let mut last = surface();
        last.frame = shepr_protocol::FrameData::blank(200, 100).expect("test frame");
        let mut next = last.clone();
        next.surface_revision = crate::test_counters::surface(2);
        for y in 0..42usize {
            for x in (0..200usize).step_by(2) {
                next.frame.cells_mut()[y * 200 + x].symbol = "z".into();
            }
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
        next.surface_revision = crate::test_counters::surface(2);
        assert!(matches!(
            message(&last, &next).expect("planning"),
            SurfaceDeltaPlan::Unchanged(ServerMessage::SurfaceUpdate(_))
        ));
    }

    #[test]
    fn unchanged_surface_with_a_full_link_table_is_reported_compactly() {
        let mut last = surface();
        last.frame
            .set_hyperlinks(vec![String::new(); shepr_protocol::MAX_SURFACE_HYPERLINKS])
            .expect("table at its budget");
        let mut next = last.clone();
        next.surface_revision = crate::test_counters::surface(2);

        assert_unchanged_update_is_compact(message(&last, &next).expect("planning"));
    }

    fn assert_unchanged_update_is_compact(plan: SurfaceDeltaPlan) {
        let SurfaceDeltaPlan::Unchanged(ServerMessage::SurfaceUpdate(update)) = plan else {
            panic!("expected unchanged surface update");
        };
        assert!(update.spans.is_empty());
        assert!(matches!(
            &update.meta,
            Some(shepr_protocol::SurfaceMeta::Patch(meta)) if meta.panes.is_empty()
        ));
        let mut bytes = Vec::new();
        shepr_protocol::write_message(&mut bytes, &ServerMessage::SurfaceUpdate(update))
            .expect("compact unchanged update encodes");
    }
}
