//! Optional endpoint encoding that retains unchanged terminal cells across projections.

use super::{CellData, PaneSurfaceFrame, ServerMessage};
use serde::{Deserialize, Serialize};

pub(crate) const MESSAGE_KIND: &str = "endpoint.surface-reuse.v1";

#[derive(Serialize, Deserialize)]
struct SurfaceReuse<S> {
    base_surface_revision: u64,
    surface: S,
}

pub(crate) fn message(
    last: &PaneSurfaceFrame,
    surface: &mut PaneSurfaceFrame,
) -> serde_json::Result<Option<ServerMessage>> {
    if !Baseline::new(
        &last.boot_id,
        last.projection_revision,
        last.surface_revision,
    )
    .accepts(
        &surface.boot_id,
        last.surface_revision,
        surface.surface_revision,
        &ProjectionUpdate::Reuse {
            next: surface.projection_revision,
        },
    ) {
        return Ok(None);
    }
    let cells = std::mem::take(&mut surface.frame.cells);
    let data = serde_json::to_string(&SurfaceReuse {
        base_surface_revision: last.surface_revision,
        surface: &*surface,
    });
    surface.frame.cells = cells;
    let message = ServerMessage::EndpointControl {
        kind: MESSAGE_KIND.into(),
        data: data?,
    };
    // JSON can expand non-cell data (for example escaped hyperlink URLs). A
    // failed compact encoding must fall back, not strand a newer snapshot.
    match super::codec::encoded_len(&message) {
        Ok(size) => Ok(super::frame_payload_fits(size).then_some(message)),
        Err(error) => {
            tracing::warn!(%error, "failed to size surface reuse");
            Ok(None)
        }
    }
}

#[derive(Default)]
struct CellBaseline {
    boot_id: String,
    projection_revision: u64,
    surface_revision: u64,
    width: u16,
    height: u16,
    cells: Vec<CellData>,
}

pub(crate) enum ProjectionUpdate {
    Reuse { next: u64 },
    Patch { revision: u64 },
    Delta { base: u64, next: u64 },
}

pub(crate) struct Baseline<'a> {
    boot_id: &'a str,
    projection_revision: u64,
    surface_revision: u64,
}

impl<'a> Baseline<'a> {
    pub(crate) fn new(boot_id: &'a str, projection_revision: u64, surface_revision: u64) -> Self {
        Self {
            boot_id,
            projection_revision,
            surface_revision,
        }
    }

    pub(crate) fn accepts(
        &self,
        boot_id: &str,
        base_surface_revision: u64,
        surface_revision: u64,
        projection: &ProjectionUpdate,
    ) -> bool {
        if self.boot_id != boot_id
            || base_surface_revision != self.surface_revision
            || self.surface_revision.checked_add(1) != Some(surface_revision)
        {
            return false;
        }
        match *projection {
            ProjectionUpdate::Reuse { next } => next >= self.projection_revision,
            ProjectionUpdate::Patch { revision } => revision == self.projection_revision,
            ProjectionUpdate::Delta { base, next } => {
                base == self.projection_revision && next >= self.projection_revision
            }
        }
    }
}

impl CellBaseline {
    fn revisions(&self) -> Baseline<'_> {
        Baseline::new(
            &self.boot_id,
            self.projection_revision,
            self.surface_revision,
        )
    }
}

/// Connection-local decoding happens before activation and presentation filtering, so
/// switching endpoints cannot discard a baseline needed by the next wire message. The
/// exact-build preamble guarantees that surface deltas are supported by both peers.
#[derive(Default)]
pub(crate) struct Decoder {
    baseline: Option<CellBaseline>,
}

impl Decoder {
    pub(crate) fn decode(&mut self, message: ServerMessage) -> Result<ServerMessage, String> {
        let message = match message {
            ServerMessage::EndpointControl { kind, data }
                if kind == super::surface_delta::MESSAGE_KIND =>
            {
                return self.decode_delta(&data).map(ServerMessage::PaneSurface);
            }
            ServerMessage::EndpointControl { kind, data } if kind == MESSAGE_KIND => {
                let reuse: SurfaceReuse<PaneSurfaceFrame> = serde_json::from_str(&data)
                    .map_err(|error| format!("invalid surface reuse: {error}"))?;
                let Some(base) = &mut self.baseline else {
                    return Err("surface reuse without a baseline".into());
                };
                let mut surface = reuse.surface;
                if !base.revisions().accepts(
                    &surface.boot_id,
                    reuse.base_surface_revision,
                    surface.surface_revision,
                    &ProjectionUpdate::Reuse {
                        next: surface.projection_revision,
                    },
                ) || base.width != surface.frame.width
                    || base.height != surface.frame.height
                    || !surface.frame.cells.is_empty()
                {
                    return Err("surface reuse does not match its baseline".into());
                }
                surface.frame.cells.clone_from(&base.cells);
                base.projection_revision = surface.projection_revision;
                base.surface_revision = surface.surface_revision;
                return Ok(ServerMessage::PaneSurface(surface));
            }
            message => message,
        };
        // The server builds every reuse, delta and patch against the surface it
        // last sent on this connection, so a full surface that does not fill its
        // own grid, or a patch that does not continue this baseline, means the
        // two sides have diverged. Fail here with the real reason, leaving the
        // baseline untouched as the reuse and delta paths do, rather than store
        // a bad grid or silently drop the baseline and fail later on the next
        // reuse with "without a baseline". The client transport treats any
        // decode error as the end of the connection.
        match &message {
            ServerMessage::PaneSurface(surface) => {
                let expected = usize::from(surface.frame.width) * usize::from(surface.frame.height);
                if surface.frame.cells.len() != expected {
                    return Err("pane surface cell count does not match its size".into());
                }
                let base = self.baseline.get_or_insert_with(CellBaseline::default);
                base.boot_id.clone_from(&surface.boot_id);
                base.projection_revision = surface.projection_revision;
                base.surface_revision = surface.surface_revision;
                base.width = surface.frame.width;
                base.height = surface.frame.height;
                base.cells.clone_from(&surface.frame.cells);
            }
            ServerMessage::PaneSurfacePatch(patch) => {
                let Some(base) = &mut self.baseline else {
                    return Err("surface patch without a baseline".into());
                };
                if !base.revisions().accepts(
                    &patch.boot_id,
                    patch.base_surface_revision,
                    patch.surface_revision,
                    &ProjectionUpdate::Patch {
                        revision: patch.projection_revision,
                    },
                ) {
                    return Err("surface patch does not match its baseline".into());
                }
                // `apply_rows` checks every span before touching the grid, so a
                // bad patch does not leave the baseline half-applied.
                super::surface_delta::apply_rows(
                    &mut base.cells,
                    base.width,
                    base.height,
                    &patch.rows,
                )
                .map_err(|error| {
                    format!("surface patch rejected against the cell baseline: {error}")
                })?;
                base.surface_revision = patch.surface_revision;
            }
            _ => {}
        }
        Ok(message)
    }

    fn decode_delta(&mut self, data: &str) -> Result<PaneSurfaceFrame, String> {
        use super::surface_delta;
        let Some(base) = &mut self.baseline else {
            return Err("surface delta without a baseline".into());
        };
        let delta = surface_delta::decode_for(data, (base.width, base.height))?;
        let mut surface = delta.surface;
        if !base.revisions().accepts(
            &surface.boot_id,
            delta.base_surface_revision,
            surface.surface_revision,
            &ProjectionUpdate::Delta {
                base: delta.base_projection_revision,
                next: surface.projection_revision,
            },
        ) || base.cells.len() != usize::from(base.width) * usize::from(base.height)
        {
            return Err("surface delta does not match its baseline".into());
        }
        surface.frame.cells.clone_from(&base.cells);
        surface_delta::apply_rows(
            &mut surface.frame.cells,
            base.width,
            base.height,
            &delta.rows,
        )?;
        if surface.frame.cells.iter().any(|cell| {
            cell.hyperlink
                .is_some_and(|index| index as usize >= surface.frame.hyperlinks.len())
        }) {
            return Err("surface delta has an invalid hyperlink index".into());
        }
        // Validate the entire update before advancing either grid or revision.
        // The same spans already applied to an identically sized copy above,
        // so this cannot fail partway through the baseline.
        surface_delta::apply_rows(&mut base.cells, base.width, base.height, &delta.rows)?;
        base.projection_revision = surface.projection_revision;
        base.surface_revision = surface.surface_revision;
        Ok(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{FrameData, PaneSurfacePatch, PaneSurfacePatchRow};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            fg: 0,
            bg: 0,
            modifier: 0,
            skip: false,
            hyperlink: None,
        }
    }

    fn surface(width: u16, height: u16) -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: 1,
            surface_revision: 1,
            frame: FrameData {
                cells: vec![cell(" "); usize::from(width) * usize::from(height)],
                width,
                height,
                cursor: None,
                hyperlinks: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    fn patch(base: u64, next: u64, rows: Vec<PaneSurfacePatchRow>) -> ServerMessage {
        ServerMessage::PaneSurfacePatch(PaneSurfacePatch {
            boot_id: "boot".into(),
            projection_revision: 1,
            base_surface_revision: base,
            surface_revision: next,
            rows,
            panes: Vec::new(),
            cursor: None,
        })
    }

    fn row(x: u16, y: u16, symbol: &str) -> PaneSurfacePatchRow {
        PaneSurfacePatchRow {
            x,
            y,
            cells: vec![cell(symbol)],
        }
    }

    fn reuse_after(decoder: &mut Decoder, base_revision: u64) -> Result<PaneSurfaceFrame, String> {
        let mut last = surface(2, 2);
        last.surface_revision = base_revision;
        let mut next = surface(2, 2);
        next.surface_revision = base_revision + 1;
        let message = message(&last, &mut next)
            .map_err(|error| error.to_string())?
            .ok_or("reuse fits in a frame")?;
        match decoder.decode(message)? {
            ServerMessage::PaneSurface(surface) => Ok(surface),
            other => Err(format!("expected a pane surface, got {other:?}")),
        }
    }

    #[test]
    fn baseline_accepts_each_update_projection_rule_in_one_place() {
        let baseline = Baseline::new("boot", 4, 9);

        let accepts = |base_surface_revision, surface_revision, projection: ProjectionUpdate| {
            baseline.accepts("boot", base_surface_revision, surface_revision, &projection)
        };
        assert!(accepts(9, 10, ProjectionUpdate::Reuse { next: 4 }));
        assert!(accepts(9, 10, ProjectionUpdate::Reuse { next: 5 }));
        assert!(!accepts(9, 10, ProjectionUpdate::Reuse { next: 3 }));
        assert!(accepts(9, 10, ProjectionUpdate::Patch { revision: 4 }));
        assert!(!accepts(9, 10, ProjectionUpdate::Patch { revision: 5 }));
        assert!(accepts(9, 10, ProjectionUpdate::Delta { base: 4, next: 5 }));
        assert!(!accepts(
            9,
            10,
            ProjectionUpdate::Delta { base: 3, next: 5 }
        ));
        assert!(!accepts(
            9,
            10,
            ProjectionUpdate::Delta { base: 4, next: 3 }
        ));
        assert!(!baseline.accepts("another-boot", 9, 10, &ProjectionUpdate::Reuse { next: 4 },));
        assert!(!accepts(8, 10, ProjectionUpdate::Reuse { next: 4 }));
        assert!(!accepts(9, 9, ProjectionUpdate::Reuse { next: 4 }));

        let exhausted = Baseline::new("boot", 4, u64::MAX);
        assert!(!exhausted.accepts(
            "boot",
            u64::MAX,
            u64::MAX,
            &ProjectionUpdate::Reuse { next: 4 },
        ));
    }

    #[test]
    fn full_surface_with_a_short_grid_is_rejected_and_not_stored() {
        let mut decoder = Decoder::default();
        let mut short = surface(2, 2);
        short.frame.cells.pop();
        let error = decoder
            .decode(ServerMessage::PaneSurface(short))
            .expect_err("a grid that does not fill its size is a protocol error");
        assert!(error.contains("cell count"), "{error}");
        assert!(
            reuse_after(&mut decoder, 1).is_err(),
            "no baseline was kept"
        );
    }

    #[test]
    fn patch_without_a_full_surface_baseline_is_rejected() {
        let mut decoder = Decoder::default();
        let error = decoder
            .decode(patch(1, 2, vec![row(0, 0, "x")]))
            .expect_err("a patch cannot establish its own baseline");
        assert!(error.contains("without a baseline"), "{error}");

        decoder
            .decode(ServerMessage::PaneSurface(surface(2, 2)))
            .expect("a later full surface establishes the baseline");
        decoder
            .decode(patch(1, 2, vec![row(0, 0, "x")]))
            .expect("patch applies after a full surface");
    }

    #[test]
    fn mismatched_patch_fails_with_its_own_reason_and_keeps_the_baseline() {
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(surface(2, 2)))
            .expect("test precondition");

        let error = decoder
            .decode(patch(7, 8, vec![row(0, 0, "x")]))
            .expect_err("a patch against another revision is a protocol error");
        assert!(error.contains("does not match its baseline"), "{error}");

        let error = decoder
            .decode(patch(1, 2, vec![row(0, 0, "x"), row(0, 2, "y")]))
            .expect_err("a patch outside the grid is a protocol error");
        assert!(error.contains("against the cell baseline"), "{error}");

        // Patches obey the same span rule as deltas: sorted, disjoint, non-empty.
        for rows in [
            vec![row(0, 1, "x"), row(0, 0, "y")],
            vec![row(0, 0, "x"), row(0, 0, "y")],
            vec![PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: Vec::new(),
            }],
        ] {
            let error = decoder
                .decode(patch(1, 2, rows))
                .expect_err("a patch breaking the span rule is a protocol error");
            assert!(error.contains("against the cell baseline"), "{error}");
        }

        // No failure touched the baseline: its first row was not
        // half-applied, and revision 1 still anchors the next update.
        let reused = reuse_after(&mut decoder, 1).expect("baseline intact");
        assert_eq!(reused.frame.cells, surface(2, 2).frame.cells);
    }

    #[test]
    fn matching_patch_advances_the_baseline() {
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(surface(2, 2)))
            .expect("test precondition");
        decoder
            .decode(patch(1, 2, vec![row(1, 1, "z")]))
            .expect("matching patch");
        let reused = reuse_after(&mut decoder, 2).expect("reuse after patch");
        assert_eq!(reused.frame.cells[3], cell("z"));
    }

    #[test]
    fn reuse_cannot_move_the_projection_revision_backwards() {
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(surface(2, 2)))
            .expect("test precondition");

        let last = surface(2, 2);
        let mut regressed = last.clone();
        regressed.projection_revision = 0;
        regressed.surface_revision = 2;
        assert!(
            message(&last, &mut regressed)
                .expect("test precondition")
                .is_none()
        );

        let mut mismatched_projection = last.clone();
        mismatched_projection.projection_revision = 0;
        mismatched_projection.surface_revision = 2;
        let reuse = ServerMessage::EndpointControl {
            kind: MESSAGE_KIND.into(),
            data: serde_json::to_string(&SurfaceReuse {
                base_surface_revision: 1,
                surface: &mismatched_projection,
            })
            .expect("test precondition"),
        };
        let error = decoder
            .decode(reuse)
            .expect_err("reuse cannot regress the projection revision");
        assert!(error.contains("does not match its baseline"), "{error}");

        let reused = reuse_after(&mut decoder, 1).expect("failed update left baseline intact");
        assert_eq!(reused.projection_revision, 1);
    }
}
