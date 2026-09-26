//! Optional sparse delivery of a completely recomputed pane surface.

use base64::{Engine as _, engine::general_purpose::STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};

use super::{CellData, PaneSurfaceFrame, PaneSurfacePatchRow, ServerMessage};

mod decode;

pub(crate) const CAPABILITY: &str = "surface_delta";
pub(crate) const MESSAGE_KIND: &str = "endpoint.surface-delta.v1";
pub(crate) const MAX_SPANS: usize = 4096;

#[derive(Serialize, Deserialize)]
pub(crate) struct SurfaceDelta<S, R = Vec<PaneSurfacePatchRow>> {
    pub(crate) base_projection_revision: u64,
    pub(crate) base_surface_revision: u64,
    pub(crate) surface: S,
    pub(crate) rows: R,
}

#[cfg(test)]
pub(crate) fn decode(data: &str) -> Result<SurfaceDelta<PaneSurfaceFrame>, String> {
    decode::decode(data, None)
}

pub(crate) fn decode_for(
    data: &str,
    expected: (u16, u16),
) -> Result<SurfaceDelta<PaneSurfaceFrame>, String> {
    decode::decode(data, Some(expected))
}

/// Copies each span into a row-major grid of `width` columns.
///
/// Fails on a span that crosses its row or the grid instead of panicking. The
/// decoder validates spans against the baseline first, so this is a backstop;
/// on failure the grid may hold the spans applied before the bad one, and the
/// caller must discard it.
pub(crate) fn apply_rows(
    cells: &mut [CellData],
    width: u16,
    rows: &[PaneSurfacePatchRow],
) -> Result<(), String> {
    for row in rows {
        let start = usize::from(row.y) * usize::from(width) + usize::from(row.x);
        let end = start.saturating_add(row.cells.len());
        let target = if usize::from(row.x) + row.cells.len() <= usize::from(width) {
            cells.get_mut(start..end)
        } else {
            None
        };
        target
            .ok_or("surface delta span exceeds the cell grid")?
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
            if rows.len() == MAX_SPANS
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
    let expected_cells = usize::from(surface.frame.width) * usize::from(surface.frame.height);
    if last.boot_id != surface.boot_id
        || last.frame.width != surface.frame.width
        || last.frame.height != surface.frame.height
        || last.frame.cells.len() != expected_cells
        || !decode::metadata_fits(surface)
    {
        return Ok(None);
    }
    let full_size = encoded_size(full)?;
    let ServerMessage::PaneSurface(surface) = full else {
        return Ok(None);
    };
    let max = super::MAX_FRAME_SIZE;
    let cells = std::mem::take(&mut surface.frame.cells);
    let encoded = (|| {
        let Some(rows) = changed_rows(&last.frame.cells, &cells, surface.frame.width, full_size)?
        else {
            return Ok(None);
        };
        let delta = SurfaceDelta {
            base_projection_revision: last.projection_revision,
            base_surface_revision: last.surface_revision,
            surface: &*surface,
            rows,
        };
        let size = encoded_size(&delta)?;
        let encoded_len = base64::encoded_len(size, false).ok_or("surface delta size overflow")?;
        if encoded_len > max || encoded_len >= full_size {
            return Ok(None);
        }
        super::codec::to_vec(&delta)
            .map(Some)
            .map_err(|error| error.to_string())
    })();
    surface.frame.cells = cells;
    let Some(bytes) = encoded? else {
        return Ok(None);
    };
    let message = ServerMessage::EndpointControl {
        kind: MESSAGE_KIND.into(),
        data: STANDARD_NO_PAD.encode(bytes),
    };
    let size = encoded_size(&message)?;
    Ok((size <= max && size < full_size).then_some(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::FrameData;
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
        let mut decoder = crate::protocol::surface_reuse::Decoder::new(true);
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
    fn apply_rows_rejects_spans_outside_the_grid_instead_of_panicking() {
        let mut cells = surface().frame.cells;
        let cell = cells[0].clone();
        let span = |x, y, len| PaneSurfacePatchRow {
            x,
            y,
            cells: vec![cell.clone(); len],
        };
        assert!(apply_rows(&mut cells, 120, &[span(119, 0, 2)]).is_err());
        assert!(apply_rows(&mut cells, 120, &[span(0, 40, 1)]).is_err());
        assert!(apply_rows(&mut cells, 120, &[span(119, 39, 1)]).is_ok());
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
                5 => corrupt
                    .surface
                    .frame
                    .cells
                    .push(next.frame.cells[0].clone()),
                6 => corrupt.rows[0].cells[0].hyperlink = Some(0),
                7 => corrupt.rows.push(corrupt.rows[0].clone()),
                8 => corrupt.surface.projection_revision = 0,
                _ => unreachable!(),
            }
            let mut decoder = crate::protocol::surface_reuse::Decoder::new(true);
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
        let mut decoder = crate::protocol::surface_reuse::Decoder::new(true);
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
        let base_revision = next.surface_revision;
        next.surface_revision += 1;
        next.projection_revision += 1;
        let reused = crate::protocol::surface_reuse::message(base_revision, &mut next)
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
    fn surface_delta_rejects_invalid_spans_atomically_and_requires_negotiation() {
        let last = surface();
        let mut next = last.clone();
        next.surface_revision += 1;
        next.frame.cells[0].symbol = "x".into();
        let update = message(&last, &mut ServerMessage::PaneSurface(next.clone()))
            .expect("test precondition")
            .expect("test precondition");
        let mut legacy = crate::protocol::surface_reuse::Decoder::default();
        legacy
            .decode(ServerMessage::PaneSurface(last.clone()))
            .expect("test precondition");
        assert!(legacy.decode(update.clone()).is_err());
        let mut decoder = crate::protocol::surface_reuse::Decoder::new(true);
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
}
