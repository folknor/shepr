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
    super::surface_grid_size(surface.frame.width, surface.frame.height)
        .is_some_and(|cells| cells == surface.frame.cells.len())
        && surface.frame.hyperlinks.len() <= MAX_SURFACE_HYPERLINKS
        && surface.panes.len() <= MAX_SURFACE_PANES
        && surface.splits.len() <= MAX_SURFACE_SPLITS
        && surface
            .splits
            .iter()
            .all(|split| split.path.len() <= MAX_SURFACE_SPLIT_PATH)
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
    if super::surface_grid_size(width, height) != Some(cells.len()) {
        return Err(SurfaceDeltaError::InvalidGrid);
    }
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

pub fn message(
    last: &PaneSurfaceFrame,
    full: &mut ServerMessage,
) -> Result<Option<ServerMessage>, SurfaceDeltaError> {
    let ServerMessage::PaneSurface(surface) = &*full else {
        return Ok(None);
    };
    let Some(expected_cells) = super::surface_grid_size(surface.frame.width, surface.frame.height)
    else {
        return Ok(None);
    };
    let baseline = super::surface_reuse::Baseline::new(
        &last.boot_id,
        last.projection_revision,
        last.surface_revision,
    );
    if !baseline.accepts_surface(surface)
        || last.frame.width != surface.frame.width
        || last.frame.height != surface.frame.height
        || last.frame.cells.len() != expected_cells
        || !metadata_fits(surface)
    {
        return Ok(None);
    }
    // Every cell has a string length prefix, two color discriminants, a skip
    // byte and a hyperlink option tag: at least five bytes, even ignoring its
    // symbol and style. This lower bound avoids counting the full grid while
    // guaranteeing any chosen cell delta is smaller. It may miss useful deltas on
    // small grids or when most of the full message consists of metadata.
    let full_size = expected_cells.saturating_mul(5);
    let ServerMessage::PaneSurface(surface) = full else {
        return Ok(None);
    };
    {
        let Some(rows) = changed_rows(&last.frame.cells, &surface.frame.cells, surface.frame.width)
        else {
            return Ok(None);
        };
        let spans = rows
            .into_iter()
            .map(|row| PaneSurfacePatchRow {
                x: row.x,
                y: row.y,
                cells: row.cells.to_vec(),
            })
            .collect();
        let update = baseline.update(surface, spans, last);
        // Metadata-only updates always retain the grid. Counting potentially
        // large projection metadata cannot improve this choice, and returning
        // it here avoids a second whole-grid equality pass in the server.
        let metadata_only = update.spans.is_empty();
        let message = ServerMessage::SurfaceUpdate(update);
        if metadata_only {
            return Ok(Some(message));
        }
        let size = encoded_size(&message)?;
        Ok((size < full_size).then_some(message))
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
    fn sparse_candidate_is_smaller_and_keeps_large_hyperlink_metadata_off_wire() {
        let mut last = surface();
        last.frame.hyperlinks = vec!["https://example.test/".repeat(4096)];
        let mut next = last.clone();
        next.surface_revision = super::super::SurfaceRevision::new(2);
        next.frame.cells[0].symbol = "z".into();
        let mut full = ServerMessage::PaneSurface(next.clone());
        let delta = message(&last, &mut full)
            .expect("planning")
            .expect("sparse delta");
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
        let mut full = ServerMessage::PaneSurface(next.clone());
        assert!(message(&last, &mut full).expect("planning").is_none());
        assert_eq!(full, ServerMessage::PaneSurface(next));
    }
}
