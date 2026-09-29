//! Optional endpoint encoding that retains unchanged terminal cells across projections.

use super::{
    BootId, CellData, PaneSurfaceFrame, PaneSurfacePatch, ProjectionRevision, PublicPaneId,
    ServerMessage, SurfaceRevision, SurfaceUpdate,
};

#[derive(Debug)]
pub enum SurfaceDecodeError {
    MissingBaseline,
    BaselineMismatch,
    MissingPatchBaseline,
    PatchBaselineMismatch,
    MissingMetadata,
    MetadataMismatch,
    InvalidHyperlink,
    InvalidDimensions,
    InvalidCellCount,
    InvalidRows(&'static str),
    Delta(super::surface_delta::SurfaceDeltaError),
    RejectedPatch(super::surface_delta::SurfaceDeltaError),
    /// Identifies the surface whose validation failed.
    WithSubject {
        subject: SurfaceDecodeSubject,
        source: Box<Self>,
    },
}

/// Wire identity known when a decoded surface fails semantic validation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceDecodeSubject {
    pub boot_id: BootId,
    pub projection_revision: ProjectionRevision,
    pub surface_revision: SurfaceRevision,
    pub pane_ids: Vec<PublicPaneId>,
}

impl std::fmt::Display for SurfaceDecodeSubject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "boot {}, projection revision {}, surface revision {}",
            self.boot_id,
            self.projection_revision.get(),
            self.surface_revision.get()
        )?;
        if !self.pane_ids.is_empty() {
            f.write_str(", panes ")?;
            let mut first = true;
            for pane_id in &self.pane_ids {
                if !first {
                    f.write_str(", ")?;
                }
                write!(f, "{pane_id}")?;
                first = false;
            }
        }
        Ok(())
    }
}

impl std::fmt::Display for SurfaceDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBaseline => f.write_str("surface update without a baseline"),
            Self::BaselineMismatch => f.write_str("surface update does not match its baseline"),
            Self::MissingPatchBaseline => f.write_str("surface patch without a baseline"),
            Self::PatchBaselineMismatch => f.write_str("surface patch does not match its baseline"),
            Self::MissingMetadata => f.write_str("surface update is missing projection metadata"),
            Self::MetadataMismatch => f.write_str("surface metadata does not match its baseline"),
            Self::InvalidHyperlink => f.write_str("surface has an invalid hyperlink index"),
            Self::InvalidDimensions => f.write_str("pane surface dimensions exceed the limit"),
            Self::InvalidCellCount => {
                f.write_str("pane surface cell count does not match its size")
            }
            Self::InvalidRows(reason) => f.write_str(reason),
            Self::Delta(error) => write!(f, "{error}"),
            Self::RejectedPatch(error) => write!(
                f,
                "surface patch rejected against the cell baseline: {error}"
            ),
            Self::WithSubject { subject, source } => write!(f, "{subject}: {source}"),
        }
    }
}

// Display includes nested causes, so leave the source chain empty to avoid repeating them.
impl std::error::Error for SurfaceDecodeError {}

impl SurfaceDecodeError {
    fn with_subject(self, subject: SurfaceDecodeSubject) -> Self {
        Self::WithSubject {
            subject,
            source: Box::new(self),
        }
    }
}

impl From<super::surface_delta::SurfaceDeltaError> for SurfaceDecodeError {
    fn from(error: super::surface_delta::SurfaceDeltaError) -> Self {
        Self::Delta(error)
    }
}

pub fn message(
    last: &PaneSurfaceFrame,
    surface: &mut PaneSurfaceFrame,
) -> Result<Option<ServerMessage>, super::codec::CodecError> {
    let baseline = Baseline::new(
        &last.boot_id,
        last.projection_revision,
        last.surface_revision,
    );
    if !baseline.accepts_surface(surface) {
        return Ok(None);
    }
    let update = baseline.update(surface, Vec::new());
    let message = ServerMessage::SurfaceUpdate(update);
    let size = super::codec::encoded_len(&message)?;
    Ok(super::frame_payload_fits(size).then_some(message))
}

#[derive(Default)]
struct CellBaseline {
    boot_id: super::BootId,
    projection_revision: ProjectionRevision,
    surface_revision: SurfaceRevision,
    width: u16,
    height: u16,
    cells: Vec<CellData>,
    meta: Option<super::surface::SurfaceMeta>,
}

impl SurfaceDecodeSubject {
    /// Names the update's panes, or the baseline's when the update keeps the
    /// previous metadata.
    fn from_update(update: &SurfaceUpdate, baseline: Option<&CellBaseline>) -> Self {
        let meta = update
            .meta
            .as_ref()
            .or_else(|| baseline.and_then(|base| base.meta.as_ref()));
        Self::from_update_header(
            &update.boot_id,
            update.projection_revision,
            update.surface_revision,
            meta.map_or(&[], |meta| meta.panes.as_slice()),
        )
    }

    fn from_update_header(
        boot_id: &BootId,
        projection_revision: ProjectionRevision,
        surface_revision: SurfaceRevision,
        panes: &[super::PaneSurfacePane],
    ) -> Self {
        Self {
            boot_id: boot_id.clone(),
            projection_revision,
            surface_revision,
            pane_ids: panes.iter().map(|pane| pane.pane_id.clone()).collect(),
        }
    }

    fn from_surface(surface: &PaneSurfaceFrame) -> Self {
        Self::from_update_header(
            &surface.boot_id,
            surface.projection_revision,
            surface.surface_revision,
            &surface.panes,
        )
    }

    /// Names the panes whose content the patch changed.
    fn from_patch(patch: &PaneSurfacePatch) -> Self {
        Self::from_update_header(
            &patch.boot_id,
            patch.projection_revision,
            patch.surface_revision,
            &patch.panes,
        )
    }
}

pub struct Baseline<'a> {
    boot_id: &'a str,
    projection_revision: ProjectionRevision,
    surface_revision: SurfaceRevision,
}

impl<'a> Baseline<'a> {
    pub fn new(
        boot_id: &'a str,
        projection_revision: ProjectionRevision,
        surface_revision: SurfaceRevision,
    ) -> Self {
        Self {
            boot_id,
            projection_revision,
            surface_revision,
        }
    }

    pub fn accepts(
        &self,
        boot_id: &str,
        base_surface_revision: SurfaceRevision,
        surface_revision: SurfaceRevision,
        base_projection_revision: ProjectionRevision,
        projection_revision: ProjectionRevision,
    ) -> bool {
        if self.boot_id != boot_id
            || base_surface_revision != self.surface_revision
            || self.surface_revision.checked_next() != Some(surface_revision)
        {
            return false;
        }
        base_projection_revision == self.projection_revision
            && projection_revision >= self.projection_revision
    }

    pub(crate) fn accepts_surface(&self, surface: &PaneSurfaceFrame) -> bool {
        self.accepts(
            &surface.boot_id,
            self.surface_revision,
            surface.surface_revision,
            self.projection_revision,
            surface.projection_revision,
        )
    }

    /// Builds an update after `accepts_surface` has accepted the new surface.
    pub(crate) fn update(
        &self,
        surface: &PaneSurfaceFrame,
        spans: Vec<super::PaneSurfacePatchRow>,
    ) -> super::SurfaceUpdate {
        super::SurfaceUpdate {
            boot_id: surface.boot_id.clone(),
            base_projection_revision: self.projection_revision,
            base_surface_revision: self.surface_revision,
            surface_revision: surface.surface_revision,
            projection_revision: surface.projection_revision,
            meta: Some(super::surface::SurfaceMeta::from(surface)),
            spans,
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
pub struct Decoder {
    baseline: Option<CellBaseline>,
}

impl Decoder {
    pub fn current_surface(&self) -> Option<PaneSurfaceFrame> {
        let base = self.baseline.as_ref()?;
        Some(base.meta.clone()?.into_surface(
            base.boot_id.clone(),
            base.projection_revision,
            base.surface_revision,
            base.cells.clone(),
        ))
    }

    pub fn decode(&mut self, message: ServerMessage) -> Result<ServerMessage, SurfaceDecodeError> {
        let message = match message {
            ServerMessage::SurfaceUpdate(update) => {
                let Some(base) = &mut self.baseline else {
                    return Err(SurfaceDecodeError::MissingBaseline
                        .with_subject(SurfaceDecodeSubject::from_update(&update, None)));
                };
                if !base.revisions().accepts(
                    &update.boot_id,
                    update.base_surface_revision,
                    update.surface_revision,
                    update.base_projection_revision,
                    update.projection_revision,
                ) || super::surface_grid_size(base.width, base.height) != Some(base.cells.len())
                {
                    return Err(SurfaceDecodeError::BaselineMismatch
                        .with_subject(SurfaceDecodeSubject::from_update(&update, Some(base))));
                }
                let meta = match update.meta {
                    Some(meta) => meta,
                    None => match base.meta.clone() {
                        Some(meta) => meta,
                        None => {
                            return Err(SurfaceDecodeError::MissingMetadata.with_subject(
                                SurfaceDecodeSubject::from_update_header(
                                    &update.boot_id,
                                    update.projection_revision,
                                    update.surface_revision,
                                    &[],
                                ),
                            ));
                        }
                    },
                };
                if meta.frame.width != base.width || meta.frame.height != base.height {
                    return Err(SurfaceDecodeError::MetadataMismatch.with_subject(
                        SurfaceDecodeSubject::from_update_header(
                            &update.boot_id,
                            update.projection_revision,
                            update.surface_revision,
                            &meta.panes,
                        ),
                    ));
                }
                // When topology and hyperlink indices are stable, forward an
                // internal patch so the client shell can update only touched
                // cells. Validate everything before changing the baseline.
                if update.projection_revision == base.projection_revision
                    && let Some(previous) = &base.meta
                    && meta.splits == previous.splits
                    && meta.frame.hyperlinks == previous.frame.hyperlinks
                    && meta.panes.len() == previous.panes.len()
                    && meta
                        .panes
                        .iter()
                        .zip(&previous.panes)
                        .all(|(next, old)| next.pane_id == old.pane_id)
                {
                    super::validate_patch_rows(base.width, base.height, &update.spans).map_err(
                        |reason| {
                            SurfaceDecodeError::InvalidRows(reason).with_subject(
                                SurfaceDecodeSubject::from_update_header(
                                    &update.boot_id,
                                    update.projection_revision,
                                    update.surface_revision,
                                    &meta.panes,
                                ),
                            )
                        },
                    )?;
                    if update.spans.iter().flat_map(|row| &row.cells).any(|cell| {
                        cell.hyperlink
                            .is_some_and(|index| index as usize >= meta.frame.hyperlinks.len())
                    }) {
                        return Err(SurfaceDecodeError::InvalidHyperlink.with_subject(
                            SurfaceDecodeSubject::from_update_header(
                                &update.boot_id,
                                update.projection_revision,
                                update.surface_revision,
                                &meta.panes,
                            ),
                        ));
                    }
                    let changed_panes = meta
                        .panes
                        .iter()
                        .zip(&previous.panes)
                        .filter(|(next, old)| next != old)
                        .map(|(next, _)| next.clone())
                        .collect();
                    let patch = super::PaneSurfacePatch {
                        boot_id: update.boot_id,
                        projection_revision: update.projection_revision,
                        base_surface_revision: update.base_surface_revision,
                        surface_revision: update.surface_revision,
                        rows: update.spans,
                        panes: changed_panes,
                        cursor: meta.frame.cursor.clone(),
                    };
                    super::surface_delta::apply_rows(
                        &mut base.cells,
                        base.width,
                        base.height,
                        &patch.rows,
                    )
                    .map_err(|error| {
                        SurfaceDecodeError::from(error)
                            .with_subject(SurfaceDecodeSubject::from_patch(&patch))
                    })?;
                    base.meta = Some(meta);
                    base.surface_revision = patch.surface_revision;
                    return Ok(ServerMessage::PaneSurfacePatch(patch));
                }
                let mut surface = meta.clone().into_surface(
                    update.boot_id,
                    update.projection_revision,
                    update.surface_revision,
                    base.cells.clone(),
                );
                super::surface_delta::apply_rows(
                    &mut surface.frame.cells,
                    base.width,
                    base.height,
                    &update.spans,
                )
                .map_err(|error| {
                    SurfaceDecodeError::from(error)
                        .with_subject(SurfaceDecodeSubject::from_surface(&surface))
                })?;
                if surface.frame.cells.iter().any(|cell| {
                    cell.hyperlink
                        .is_some_and(|index| index as usize >= surface.frame.hyperlinks.len())
                }) {
                    return Err(SurfaceDecodeError::InvalidHyperlink
                        .with_subject(SurfaceDecodeSubject::from_surface(&surface)));
                }
                base.cells.clone_from(&surface.frame.cells);
                base.meta = Some(meta);
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
                let Some(expected) =
                    super::surface_grid_size(surface.frame.width, surface.frame.height)
                else {
                    return Err(SurfaceDecodeError::InvalidDimensions
                        .with_subject(SurfaceDecodeSubject::from_surface(surface)));
                };
                if surface.frame.cells.len() != expected {
                    return Err(SurfaceDecodeError::InvalidCellCount
                        .with_subject(SurfaceDecodeSubject::from_surface(surface)));
                }
                if surface.frame.cells.iter().any(|cell| {
                    cell.hyperlink
                        .is_some_and(|index| index as usize >= surface.frame.hyperlinks.len())
                }) {
                    return Err(SurfaceDecodeError::InvalidHyperlink
                        .with_subject(SurfaceDecodeSubject::from_surface(surface)));
                }
                let base = self.baseline.get_or_insert_with(CellBaseline::default);
                base.boot_id.clone_from(&surface.boot_id);
                base.projection_revision = surface.projection_revision;
                base.surface_revision = surface.surface_revision;
                base.width = surface.frame.width;
                base.height = surface.frame.height;
                base.cells.clone_from(&surface.frame.cells);
                base.meta = Some(surface.into());
            }
            ServerMessage::PaneSurfacePatch(patch) => {
                let Some(base) = &mut self.baseline else {
                    return Err(SurfaceDecodeError::MissingPatchBaseline
                        .with_subject(SurfaceDecodeSubject::from_patch(patch)));
                };
                if !base.revisions().accepts(
                    &patch.boot_id,
                    patch.base_surface_revision,
                    patch.surface_revision,
                    base.projection_revision,
                    patch.projection_revision,
                ) {
                    return Err(SurfaceDecodeError::PatchBaselineMismatch
                        .with_subject(SurfaceDecodeSubject::from_patch(patch)));
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
                    SurfaceDecodeError::RejectedPatch(error)
                        .with_subject(SurfaceDecodeSubject::from_patch(patch))
                })?;
                base.surface_revision = patch.surface_revision;
            }
            _ => {}
        }
        Ok(message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameData, PaneSurfacePatchRow, SurfaceUpdate, WireColor, WireStyle};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: WireStyle::default(),
            skip: false,
            hyperlink: None,
        }
    }

    fn surface() -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: "boot".into(),
            projection_revision: crate::ProjectionRevision::new(1),
            surface_revision: crate::SurfaceRevision::new(1),
            frame: FrameData {
                cells: vec![cell("a"), cell("b")],
                width: 2,
                height: 1,
                cursor: None,
                hyperlinks: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    #[test]
    fn surface_update_applies_cells_and_projection_atomically() {
        let mut decoder = Decoder::default();
        let first = surface();
        decoder
            .decode(ServerMessage::PaneSurface(first.clone()))
            .expect("baseline");
        let update = SurfaceUpdate {
            boot_id: first.boot_id.clone(),
            base_surface_revision: crate::SurfaceRevision::new(1),
            surface_revision: crate::SurfaceRevision::new(2),
            base_projection_revision: crate::ProjectionRevision::new(1),
            projection_revision: crate::ProjectionRevision::new(2),
            meta: None,
            spans: vec![PaneSurfacePatchRow {
                x: 1,
                y: 0,
                cells: vec![cell("c")],
            }],
        };
        let mut bad = update.clone();
        bad.spans.push(bad.spans[0].clone());
        assert!(decoder.decode(ServerMessage::SurfaceUpdate(bad)).is_err());
        let ServerMessage::PaneSurface(applied) = decoder
            .decode(ServerMessage::SurfaceUpdate(update))
            .expect("valid update after rejection")
        else {
            panic!("expected surface");
        };
        assert_eq!(applied.frame.cells[1], cell("c"));
        assert_eq!(applied.projection_revision, 2);
        assert_eq!(applied.surface_revision, 2);
    }

    #[test]
    fn surface_update_rejects_a_stale_baseline() {
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(surface()))
            .expect("baseline");
        let update = SurfaceUpdate {
            boot_id: "boot".into(),
            base_surface_revision: crate::SurfaceRevision::new(0),
            surface_revision: crate::SurfaceRevision::new(2),
            base_projection_revision: crate::ProjectionRevision::new(1),
            projection_revision: crate::ProjectionRevision::new(1),
            meta: None,
            spans: Vec::new(),
        };
        let error = decoder
            .decode(ServerMessage::SurfaceUpdate(update))
            .expect_err("stale baseline is rejected");
        let SurfaceDecodeError::WithSubject { subject, source } = error else {
            panic!("surface errors retain their subject");
        };
        assert!(matches!(
            source.as_ref(),
            SurfaceDecodeError::BaselineMismatch
        ));
        assert_eq!(subject.boot_id, "boot");
        assert_eq!(subject.projection_revision, 1);
        assert_eq!(subject.surface_revision, 2);
    }

    #[test]
    fn surface_update_keeps_same_projection_as_an_internal_patch() {
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(surface()))
            .expect("baseline");
        let update = SurfaceUpdate {
            boot_id: "boot".into(),
            base_surface_revision: crate::SurfaceRevision::new(1),
            surface_revision: crate::SurfaceRevision::new(2),
            base_projection_revision: crate::ProjectionRevision::new(1),
            projection_revision: crate::ProjectionRevision::new(1),
            meta: None,
            spans: vec![PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![cell("z")],
            }],
        };
        let ServerMessage::PaneSurfacePatch(patch) = decoder
            .decode(ServerMessage::SurfaceUpdate(update))
            .expect("valid update")
        else {
            panic!("expected patch");
        };
        assert_eq!(patch.rows.len(), 1);
        assert_eq!(
            decoder.current_surface().expect("baseline").frame.cells[0],
            cell("z")
        );
    }
}
