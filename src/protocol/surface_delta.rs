//! Optional sparse delivery of a completely recomputed pane surface.

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};

use super::wire::PaneSurfaceDeltaMetadata;
use super::{
    CellData, MAX_FRAME_SIZE, MAX_SURFACE_HYPERLINKS, MAX_SURFACE_PANES, MAX_SURFACE_PATCH_SPANS,
    MAX_SURFACE_SPLIT_PATH, MAX_SURFACE_SPLITS, PaneSurfaceFrame, PaneSurfacePatchRow,
    ServerMessage,
};

pub(crate) const MESSAGE_KIND: &str = "endpoint.surface-delta.v1";

#[derive(Serialize, Deserialize)]
#[serde(bound(
    serialize = "S: Serialize, R: Serialize",
    deserialize = "S: Deserialize<'de>, R: Deserialize<'de>"
))]
pub(crate) struct SurfaceDelta<S, R = PaneSurfacePatchRow> {
    pub(crate) base_projection_revision: u64,
    pub(crate) base_surface_revision: u64,
    pub(crate) surface: S,
    #[serde(
        serialize_with = "super::codec::serialize_bounded_vec::<MAX_SURFACE_PATCH_SPANS, _, _>",
        deserialize_with = "super::codec::deserialize_bounded_vec::<MAX_SURFACE_PATCH_SPANS, _, _>"
    )]
    pub(crate) rows: Vec<R>,
}

#[cfg(test)]
pub(crate) fn decode(data: &str) -> Result<SurfaceDelta<PaneSurfaceDeltaMetadata>, String> {
    decode_delta(data, None)
}

pub(crate) fn decode_for(
    data: &str,
    expected: (u16, u16),
) -> Result<SurfaceDelta<PaneSurfaceDeltaMetadata>, String> {
    decode_delta(data, Some(expected))
}

fn decode_delta(
    data: &str,
    expected: Option<(u16, u16)>,
) -> Result<SurfaceDelta<PaneSurfaceDeltaMetadata>, String> {
    let max_encoded_len =
        base64::encoded_len(MAX_FRAME_SIZE, false).ok_or("surface delta frame limit overflow")?;
    if data.len() > max_encoded_len {
        return Err("surface delta exceeds the frame limit".into());
    }
    let bytes = STANDARD_NO_PAD
        .decode(data)
        .map_err(|error| error.to_string())?;
    let delta: SurfaceDelta<PaneSurfaceDeltaMetadata> =
        super::codec::from_slice_exact(&bytes).map_err(|error| error.to_string())?;
    let width = delta.surface.frame.width;
    let height = delta.surface.frame.height;
    let cell_budget = super::surface_grid_size(width, height)
        .ok_or("surface dimensions or cell count exceed the limit")?;
    if expected.is_some_and(|dimensions| dimensions != (width, height)) {
        return Err("surface dimensions do not match the baseline".into());
    }
    let mut span_check = super::PatchSpanCheck::new(width, height);
    let mut total_cells = 0usize;
    for row in &delta.rows {
        span_check
            .push(row.x, row.y, row.cells.len())
            .map_err(str::to_owned)?;
        total_cells = total_cells
            .checked_add(row.cells.len())
            .ok_or("surface delta cell budget overflow")?;
        if total_cells > cell_budget {
            return Err("surface delta cell budget exceeded".into());
        }
    }
    Ok(delta)
}

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
) -> Result<(), String> {
    if super::surface_grid_size(width, height) != Some(cells.len()) {
        return Err("cell grid does not match its dimensions".into());
    }
    crate::protocol::validate_patch_rows(width, height, rows)?;
    for row in rows {
        let start = usize::from(row.y) * usize::from(width) + usize::from(row.x);
        let end = start + row.cells.len();
        cells
            .get_mut(start..end)
            .ok_or("patch span exceeds the cell grid")?
            .clone_from_slice(&row.cells);
    }
    Ok(())
}

#[derive(Serialize)]
struct CellSpan<'a> {
    x: u16,
    y: u16,
    cells: &'a [CellData],
}

fn changed_rows<'a>(
    last: &[CellData],
    next: &'a [CellData],
    width: u16,
    full_size: usize,
) -> Result<Option<Vec<CellSpan<'a>>>, String> {
    let mut rows = Vec::new();
    if width == 0 {
        return Ok(Some(rows));
    }
    let mut size = 0;
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
            size += encoded_size(&span)?;
            // This lower bound excludes metadata, so aborting cannot discard a smaller delta.
            if rows.len() == MAX_SURFACE_PATCH_SPANS
                || base64::encoded_len(size, false).is_none_or(|size| size >= full_size)
            {
                return Ok(None);
            }
            rows.push(span);
        }
    }
    Ok(Some(rows))
}

fn encoded_size(value: &impl Serialize) -> Result<usize, String> {
    super::codec::encoded_len(value).map_err(|error| error.to_string())
}

pub(crate) fn message(
    last: &PaneSurfaceFrame,
    full: &mut ServerMessage,
) -> Result<Option<ServerMessage>, String> {
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
    if !baseline.accepts(
        &surface.boot_id,
        last.surface_revision,
        surface.surface_revision,
        &super::surface_reuse::ProjectionUpdate::Delta {
            base: last.projection_revision,
            next: surface.projection_revision,
        },
    ) || last.frame.width != surface.frame.width
        || last.frame.height != surface.frame.height
        || last.frame.cells.len() != expected_cells
        || !metadata_fits(surface)
    {
        return Ok(None);
    }
    let full_size = encoded_size(full)?;
    let ServerMessage::PaneSurface(surface) = full else {
        return Ok(None);
    };
    let encoded = (|| {
        let Some(rows) = changed_rows(
            &last.frame.cells,
            &surface.frame.cells,
            surface.frame.width,
            full_size,
        )?
        else {
            return Ok(None);
        };
        let delta = SurfaceDelta {
            base_projection_revision: last.projection_revision,
            base_surface_revision: last.surface_revision,
            surface: PaneSurfaceDeltaMetadata::from(&*surface),
            rows,
        };
        let size = encoded_size(&delta)?;
        let encoded_len = base64::encoded_len(size, false).ok_or("surface delta size overflow")?;
        if !super::frame_payload_fits(encoded_len) || encoded_len >= full_size {
            return Ok(None);
        }
        super::codec::to_vec(&delta)
            .map(Some)
            .map_err(|error| error.to_string())
    })();
    let Some(bytes) = encoded? else {
        return Ok(None);
    };
    let message = ServerMessage::EndpointControl {
        kind: MESSAGE_KIND.into(),
        data: STANDARD_NO_PAD.encode(bytes),
    };
    let size = encoded_size(&message)?;
    Ok((super::frame_payload_fits(size) && size < full_size).then_some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FrameData;
    use crate::protocol::{MAX_SURFACE_CELLS, MAX_SURFACE_DIMENSION};
    use ratatui::{buffer::Buffer, layout::Rect};

    fn surface() -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData::from_ratatui_buffer(&Buffer::empty(Rect::new(0, 0, 120, 40)), None),
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    fn framed_len(message: &ServerMessage) -> usize {
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, message).expect("test precondition");
        bytes.len()
    }

    fn reconstruct(last: &PaneSurfaceFrame, next: &PaneSurfaceFrame) -> ServerMessage {
        let mut decoder = crate::protocol::surface_reuse::Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(last.clone()))
            .expect("test precondition");
        let update = message(last, &mut ServerMessage::PaneSurface(next.clone()))
            .expect("test precondition")
            .expect("smaller delta");
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, &update).expect("test precondition");
        let wire =
            crate::protocol::read_message(&mut bytes.as_slice(), crate::protocol::MAX_FRAME_SIZE)
                .expect("test precondition");
        let ServerMessage::PaneSurface(decoded) = decoder.decode(wire).expect("test precondition")
        else {
            panic!("full surface");
        };
        assert_eq!(&decoded, next);
        update
    }

    #[test]
    fn apply_rows_rejects_invalid_spans_without_touching_the_grid() {
        let mut cells = surface().frame.cells;
        let original = cells.clone();
        let mut changed = cells[0].clone();
        changed.symbol = "#".into();
        let span = |x, y, len| PaneSurfacePatchRow {
            x,
            y,
            cells: vec![changed.clone(); len],
        };
        for rows in [
            vec![span(119, 0, 2)],
            vec![span(0, 40, 1)],
            vec![span(0, 0, 0)],
            // A valid span followed by one that overlaps it or is out of order.
            vec![span(0, 0, 3), span(2, 0, 1)],
            vec![span(0, 1, 1), span(0, 0, 1)],
        ] {
            assert!(apply_rows(&mut cells, 120, 40, &rows).is_err(), "{rows:?}");
            assert_eq!(cells, original, "a rejected set must not be half-applied");
        }
        assert!(apply_rows(&mut cells, 120, 40, &[span(0, 0, 3), span(3, 0, 1)]).is_ok());
        assert!(apply_rows(&mut cells, 120, 40, &[span(119, 39, 1)]).is_ok());
    }

    #[test]
    fn surface_delta_keeps_a_cell_plus_projection_update_small() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        next.projection_revision += 1;
        next.frame.cells[0].symbol = "x".into();
        reconstruct(&last, &next);
        let expected = next.clone();
        let mut full = ServerMessage::PaneSurface(next);
        let update = message(&last, &mut full)
            .expect("test precondition")
            .expect("sparse delta");
        assert!(framed_len(&update) < 1000);
        let ServerMessage::PaneSurface(next) = full else {
            panic!("full fallback");
        };
        assert_eq!(next, expected, "encoding must not consume the target grid");
    }

    #[test]
    fn surface_delta_reconstructs_cursor_and_hyperlinks() {
        use crate::protocol::CursorState;
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        next.projection_revision += 1;
        next.frame.cursor = Some(CursorState {
            x: 3,
            y: 2,
            visible: true,
            shape: 1,
        });
        next.frame.hyperlinks = vec!["https://example.com/\"quoted\"".into()];
        next.frame.cells[0].hyperlink = Some(0);
        assert!(framed_len(&reconstruct(&last, &next)) < 1000);
        let mut changed = next.clone();
        changed.surface_revision += 1;
        changed.frame.hyperlinks[0] = "https://other.example".into();
        reconstruct(&next, &changed);
    }

    #[test]
    fn surface_delta_uses_full_for_resize_and_when_full_is_smaller() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        for cell in &mut next.frame.cells {
            cell.symbol = "x".into();
        }
        assert!(
            message(&last, &mut ServerMessage::PaneSurface(next.clone()))
                .expect("test precondition")
                .is_none()
        );
        next.frame.width += 1;
        assert!(
            message(&last, &mut ServerMessage::PaneSurface(next))
                .expect("test precondition")
                .is_none()
        );
    }

    #[test]
    fn surface_delta_rejects_wrong_baselines_and_invalid_metadata_without_advancing() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        next.frame.cells[0].symbol = "x".into();
        let update = reconstruct(&last, &next);
        let ServerMessage::EndpointControl { data, .. } = &update else {
            panic!("delta");
        };
        for case in 0..9 {
            let mut corrupt = decode(data).expect("test precondition");
            match case {
                0 => corrupt.base_surface_revision += 1,
                1 => corrupt.base_projection_revision += 1,
                2 => corrupt.surface.surface_revision += 1,
                3 => corrupt.surface.boot_id = "different boot".into(),
                4 => corrupt.surface.frame.width += 1,
                5 => corrupt.surface.frame.height += 1,
                6 => corrupt.rows[0].cells[0].hyperlink = Some(0),
                7 => corrupt.rows.push(corrupt.rows[0].clone()),
                8 => corrupt.surface.projection_revision = 0,
                _ => unreachable!(),
            }
            let mut decoder = crate::protocol::surface_reuse::Decoder::default();
            decoder
                .decode(ServerMessage::PaneSurface(last.clone()))
                .expect("test precondition");
            let bad = ServerMessage::EndpointControl {
                kind: MESSAGE_KIND.into(),
                data: STANDARD_NO_PAD
                    .encode(crate::protocol::codec::to_vec(&corrupt).expect("test precondition")),
            };
            assert!(decoder.decode(bad).is_err(), "case {case}");
            let ServerMessage::PaneSurface(decoded) =
                decoder.decode(update.clone()).expect("test precondition")
            else {
                panic!("full reconstruction");
            };
            assert_eq!(decoded, next);
        }
    }

    #[test]
    fn surface_delta_shares_baselines_with_legacy_patches_and_reuse() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        next.frame.cells[0].symbol = "a".into();
        let mut decoder = crate::protocol::surface_reuse::Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(last))
            .expect("test precondition");
        decoder
            .decode(ServerMessage::PaneSurfacePatch(
                crate::protocol::PaneSurfacePatch {
                    boot_id: next.boot_id.clone(),
                    projection_revision: next.projection_revision,
                    base_surface_revision: 1,
                    surface_revision: 2,
                    rows: vec![PaneSurfacePatchRow {
                        x: 0,
                        y: 0,
                        cells: vec![next.frame.cells[0].clone()],
                    }],
                    panes: Vec::new(),
                    cursor: None,
                },
            ))
            .expect("test precondition");
        let last = next.clone();
        next.surface_revision += 1;
        next.projection_revision += 1;
        let reused = crate::protocol::surface_reuse::message(&last, &mut next)
            .expect("test precondition")
            .expect("test precondition");
        decoder.decode(reused).expect("test precondition");
        let mut latest = next.clone();
        latest.surface_revision += 1;
        latest.projection_revision += 1;
        latest.frame.cells[1].symbol = "b".into();
        let update = message(&next, &mut ServerMessage::PaneSurface(latest.clone()))
            .expect("test precondition")
            .expect("test precondition");
        let ServerMessage::PaneSurface(decoded) =
            decoder.decode(update).expect("test precondition")
        else {
            panic!("full reconstruction");
        };
        assert_eq!(decoded, latest);
    }

    #[test]
    fn surface_delta_checkerboard_borrows_cells_and_bounds_run_planning() {
        let last = surface();
        let mut next = last.clone();
        for (index, cell) in next.frame.cells.iter_mut().enumerate() {
            if index % 2 == 0 {
                cell.symbol = "x".into();
            }
        }
        let spans = changed_rows(&last.frame.cells, &next.frame.cells, 120, usize::MAX)
            .expect("test precondition")
            .expect("test precondition");
        assert_eq!(spans.len(), next.frame.cells.len() / 2);
        for span in spans {
            let start = usize::from(span.y) * 120 + usize::from(span.x);
            assert_eq!(span.cells.as_ptr(), next.frame.cells[start..].as_ptr());
        }
        assert!(
            changed_rows(&last.frame.cells, &next.frame.cells, 120, 1)
                .expect("test precondition")
                .is_none()
        );
        let mut large =
            FrameData::from_ratatui_buffer(&Buffer::empty(Rect::new(0, 0, 120, 100)), None);
        let baseline = large.cells.clone();
        for (index, cell) in large.cells.iter_mut().enumerate() {
            if index % 2 == 0 {
                cell.symbol = "x".into();
            }
        }
        assert!(
            changed_rows(&baseline, &large.cells, 120, usize::MAX)
                .expect("test precondition")
                .is_none()
        );
    }

    #[test]
    fn surface_delta_rejects_invalid_spans_atomically() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        next.frame.cells[0].symbol = "x".into();
        let update = message(&last, &mut ServerMessage::PaneSurface(next.clone()))
            .expect("test precondition")
            .expect("test precondition");
        let mut decoder = crate::protocol::surface_reuse::Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(last))
            .expect("test precondition");
        let ServerMessage::EndpointControl { data, .. } = &update else {
            panic!("delta");
        };
        let mut corrupt = decode(data).expect("test precondition");
        corrupt.rows.push(PaneSurfacePatchRow {
            x: 0,
            y: 40,
            cells: vec![next.frame.cells[0].clone()],
        });
        let bad = ServerMessage::EndpointControl {
            kind: MESSAGE_KIND.into(),
            data: STANDARD_NO_PAD
                .encode(crate::protocol::codec::to_vec(&corrupt).expect("test precondition")),
        };
        assert!(decoder.decode(bad).is_err());
        let ServerMessage::PaneSurface(decoded) =
            decoder.decode(update.clone()).expect("test precondition")
        else {
            panic!("recovered");
        };
        assert_eq!(decoded, next);
        assert!(
            decoder.decode(update.clone()).is_err(),
            "duplicate revision"
        );
        assert!(decode("not-base64!").is_err());
        let mut trailing = STANDARD_NO_PAD.decode(data).expect("test precondition");
        trailing.push(0);
        assert!(decode(&STANDARD_NO_PAD.encode(trailing)).is_err());
    }

    #[test]
    fn sender_eligibility_uses_the_shared_surface_limits() {
        let mut exact = surface();
        exact.frame.width = 1024;
        exact.frame.height = 128;
        exact.frame.cells = vec![exact.frame.cells[0].clone(); MAX_SURFACE_CELLS];
        assert!(metadata_fits(&exact));
        exact.frame.cells.pop();
        assert!(!metadata_fits(&exact));

        let mut candidate = surface();
        candidate.frame.width = MAX_SURFACE_DIMENSION + 1;
        candidate.frame.height = 1;
        candidate.frame.cells = vec![candidate.frame.cells[0].clone()];
        assert!(!metadata_fits(&candidate));
        candidate.frame.width = 1;
        candidate.frame.hyperlinks = vec![String::new(); MAX_SURFACE_HYPERLINKS + 1];
        assert!(!metadata_fits(&candidate));
    }

    #[test]
    fn base64_input_limit_uses_the_encoded_frame_budget() {
        let between_raw_and_encoded_limit = "!".repeat(MAX_FRAME_SIZE + 1);
        let error = decode(&between_raw_and_encoded_limit)
            .err()
            .expect("base64 validation should run before the encoded-size cap");
        assert!(!error.contains("exceeds the frame limit"), "{error}");

        let max_encoded_len = base64::encoded_len(MAX_FRAME_SIZE, false)
            .expect("the frame size has a representable base64 length");
        let over_encoded_limit = "!".repeat(max_encoded_len + 1);
        let error = decode(&over_encoded_limit)
            .err()
            .expect("input beyond the encoded frame budget is rejected");
        assert!(error.contains("exceeds the frame limit"), "{error}");
    }
}
