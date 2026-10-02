//! Optional endpoint encoding that retains unchanged terminal cells across projections.

use super::{
    BootId, CellData, PaneSurfaceFrame, PaneSurfacePatch, ProjectionRevision, PublicPaneId,
    ServerMessage, SurfaceRevision, SurfaceUpdate,
};

#[derive(Debug)]
pub enum SurfaceDecodeError {
    MissingBaseline,
    BaselineMismatch,
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

/// Applies a decoded patch to its complete surface baseline.
///
/// The decoder validates wire deltas against its connection-local cell grid. Client activation
/// evidence can outlive the presentation state that normally applies patches, so it uses this
/// helper to keep its full-surface baseline at the same revision as the decoder.
pub fn apply_patch_to_surface(
    surface: &mut PaneSurfaceFrame,
    patch: &PaneSurfacePatch,
) -> Result<(), SurfaceDecodeError> {
    if patch.boot_id != surface.boot_id
        || patch.projection_revision != surface.projection_revision
        || patch.base_surface_revision != surface.surface_revision
        || surface.surface_revision.checked_next() != Some(patch.surface_revision)
    {
        return Err(patch_error(
            patch,
            SurfaceDecodeError::PatchBaselineMismatch,
        ));
    }
    let Some(expected_cells) = super::surface_grid_size(surface.frame.width, surface.frame.height)
    else {
        return Err(patch_error(patch, SurfaceDecodeError::InvalidDimensions));
    };
    if surface.frame.cells.len() != expected_cells {
        return Err(patch_error(patch, SurfaceDecodeError::InvalidCellCount));
    }
    if patch.panes.iter().any(|updated| {
        !surface
            .panes
            .iter()
            .any(|existing| existing.pane_id == updated.pane_id)
    }) {
        return Err(patch_error(patch, SurfaceDecodeError::MetadataMismatch));
    }
    super::surface_delta::apply_rows(
        &mut surface.frame.cells,
        surface.frame.width,
        surface.frame.height,
        &patch.rows,
    )
    .map_err(|error| patch_error(patch, SurfaceDecodeError::RejectedPatch(error)))?;
    for updated in &patch.panes {
        if let Some(existing) = surface
            .panes
            .iter_mut()
            .find(|existing| existing.pane_id == updated.pane_id)
        {
            *existing = updated.clone();
        }
    }
    surface.frame.cursor = patch.cursor.clone();
    surface.surface_revision = patch.surface_revision;
    Ok(())
}

fn patch_error(patch: &PaneSurfacePatch, error: SurfaceDecodeError) -> SurfaceDecodeError {
    error.with_subject(SurfaceDecodeSubject::from_patch(patch))
}

impl From<super::surface_delta::SurfaceDeltaError> for SurfaceDecodeError {
    fn from(error: super::surface_delta::SurfaceDeltaError) -> Self {
        Self::Delta(error)
    }
}

struct CellBaseline {
    boot_id: super::BootId,
    projection_revision: ProjectionRevision,
    surface_revision: SurfaceRevision,
    width: u16,
    height: u16,
    cells: Vec<CellData>,
    meta: Option<super::surface::SurfaceProjectionMeta>,
}

impl SurfaceDecodeSubject {
    /// Names the update's panes, or the baseline's when the update keeps the
    /// previous metadata.
    fn from_update(update: &SurfaceUpdate, baseline: Option<&CellBaseline>) -> Self {
        let panes = match update.meta.as_ref() {
            Some(super::SurfaceMeta::Projection(meta)) => meta.panes.as_slice(),
            Some(super::SurfaceMeta::Patch(meta)) => meta.panes.as_slice(),
            None => baseline
                .and_then(|base| base.meta.as_ref())
                .map_or(&[][..], |meta| meta.panes.as_slice()),
        };
        Self::from_update_header(
            &update.boot_id,
            update.projection_revision,
            update.surface_revision,
            panes,
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
        last: &PaneSurfaceFrame,
    ) -> super::SurfaceUpdate {
        super::SurfaceUpdate {
            boot_id: surface.boot_id.clone(),
            base_projection_revision: self.projection_revision,
            base_surface_revision: self.surface_revision,
            surface_revision: surface.surface_revision,
            projection_revision: surface.projection_revision,
            meta: Some(
                if surface.projection_revision == last.projection_revision
                    && surface.frame.width == last.frame.width
                    && surface.frame.height == last.frame.height
                    && surface.frame.hyperlinks == last.frame.hyperlinks
                    && surface.splits == last.splits
                    && surface.panes.len() == last.panes.len()
                    && surface
                        .panes
                        .iter()
                        .zip(&last.panes)
                        .all(|(next, old)| next.pane_id == old.pane_id)
                {
                    super::SurfaceMeta::Patch(super::SurfacePatchMeta {
                        cursor: surface.frame.cursor.clone(),
                        panes: surface
                            .panes
                            .iter()
                            .zip(&last.panes)
                            .filter(|(next, old)| next != old)
                            .map(|(next, _)| next.clone())
                            .collect(),
                    })
                } else {
                    super::SurfaceMeta::from(surface)
                },
            ),
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
///
/// The result is client-side state after interpreting a wire message. It is deliberately
/// separate from `ServerMessage`, whose variants are exactly the messages sent on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedServerMessage {
    Wire(ServerMessage),
    /// A client-local patch produced while decoding a wire `SurfaceUpdate`.
    PaneSurfacePatch(PaneSurfacePatch),
}

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

    pub fn decode(
        &mut self,
        message: ServerMessage,
    ) -> Result<DecodedServerMessage, SurfaceDecodeError> {
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
                // Compact metadata makes retained topology explicit. Do not clone or
                // compare the baseline hyperlink table on a terminal dirty-row update.
                if !matches!(&update.meta, Some(super::SurfaceMeta::Projection(_)))
                    && update.projection_revision == base.projection_revision
                {
                    let Some(previous) = base.meta.as_mut() else {
                        return Err(SurfaceDecodeError::MissingMetadata.with_subject(
                            SurfaceDecodeSubject::from_update_header(
                                &update.boot_id,
                                update.projection_revision,
                                update.surface_revision,
                                &[],
                            ),
                        ));
                    };
                    let compact = match update.meta {
                        Some(super::SurfaceMeta::Patch(meta)) => meta,
                        None => super::SurfacePatchMeta {
                            cursor: previous.frame.cursor.clone(),
                            panes: Vec::new(),
                        },
                        // The compact branch excludes projection metadata. Keep
                        // the exhaustive fallback attributable if that guard changes.
                        Some(super::SurfaceMeta::Projection(meta)) => {
                            return Err(SurfaceDecodeError::MetadataMismatch.with_subject(
                                SurfaceDecodeSubject::from_update_header(
                                    &update.boot_id,
                                    update.projection_revision,
                                    update.surface_revision,
                                    &meta.panes,
                                ),
                            ));
                        }
                    };
                    let patch = PaneSurfacePatch {
                        boot_id: update.boot_id,
                        projection_revision: update.projection_revision,
                        base_surface_revision: update.base_surface_revision,
                        surface_revision: update.surface_revision,
                        rows: update.spans,
                        panes: compact.panes,
                        cursor: compact.cursor,
                    };
                    if patch.panes.iter().any(|updated| {
                        !previous
                            .panes
                            .iter()
                            .any(|pane| pane.pane_id == updated.pane_id)
                    }) {
                        return Err(patch_error(&patch, SurfaceDecodeError::MetadataMismatch));
                    }
                    if patch.rows.iter().flat_map(|row| &row.cells).any(|cell| {
                        cell.hyperlink
                            .is_some_and(|index| index as usize >= previous.frame.hyperlinks.len())
                    }) {
                        return Err(patch_error(&patch, SurfaceDecodeError::InvalidHyperlink));
                    }
                    super::surface_delta::apply_rows(
                        &mut base.cells,
                        base.width,
                        base.height,
                        &patch.rows,
                    )
                    .map_err(|error| patch_error(&patch, SurfaceDecodeError::from(error)))?;
                    for updated in &patch.panes {
                        if let Some(pane) = previous
                            .panes
                            .iter_mut()
                            .find(|pane| pane.pane_id == updated.pane_id)
                        {
                            pane.clone_from(updated);
                        }
                    }
                    previous.frame.cursor.clone_from(&patch.cursor);
                    base.surface_revision = patch.surface_revision;
                    return Ok(DecodedServerMessage::PaneSurfacePatch(patch));
                }
                let meta = match update.meta {
                    Some(super::SurfaceMeta::Projection(meta)) => meta,
                    Some(super::SurfaceMeta::Patch(_)) => {
                        return Err(SurfaceDecodeError::MetadataMismatch.with_subject(
                            SurfaceDecodeSubject::from_update_header(
                                &update.boot_id,
                                update.projection_revision,
                                update.surface_revision,
                                &[],
                            ),
                        ));
                    }
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
                    return Ok(DecodedServerMessage::PaneSurfacePatch(patch));
                }
                // Validate the resulting hyperlink indices before mutating the grid.
                // Unchanged intervals use the baseline; changed intervals use the spans.
                // This preserves rejection atomicity without making a scratch grid.
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
                let invalid = |cells: &[CellData]| {
                    cells.iter().any(|cell| {
                        cell.hyperlink
                            .is_some_and(|index| index as usize >= meta.frame.hyperlinks.len())
                    })
                };
                let mut end = 0;
                for row in &update.spans {
                    let start = usize::from(row.y) * usize::from(base.width) + usize::from(row.x);
                    if invalid(&base.cells[end..start]) || invalid(&row.cells) {
                        return Err(SurfaceDecodeError::InvalidHyperlink.with_subject(
                            SurfaceDecodeSubject::from_update_header(
                                &update.boot_id,
                                update.projection_revision,
                                update.surface_revision,
                                &meta.panes,
                            ),
                        ));
                    }
                    end = start + row.cells.len();
                }
                if invalid(&base.cells[end..]) {
                    return Err(SurfaceDecodeError::InvalidHyperlink.with_subject(
                        SurfaceDecodeSubject::from_update_header(
                            &update.boot_id,
                            update.projection_revision,
                            update.surface_revision,
                            &meta.panes,
                        ),
                    ));
                }
                super::surface_delta::apply_rows(
                    &mut base.cells,
                    base.width,
                    base.height,
                    &update.spans,
                )
                .map_err(SurfaceDecodeError::from)?;
                // The decoder retains its cells for later updates while the
                // caller owns the returned surface. FrameData uses Vec, so these
                // independent owners require one grid copy until cell storage is
                // shared or copy-on-write.
                let surface = meta.clone().into_surface(
                    update.boot_id,
                    update.projection_revision,
                    update.surface_revision,
                    base.cells.clone(),
                );
                base.meta = Some(meta);
                base.projection_revision = surface.projection_revision;
                base.surface_revision = surface.surface_revision;
                return Ok(DecodedServerMessage::Wire(ServerMessage::PaneSurface(
                    surface,
                )));
            }
            message => message,
        };
        // A full surface that does not fill its own grid means the two sides
        // have diverged. Fail here with the real reason rather than store a bad
        // grid or silently drop the baseline and fail later on the next reuse
        // with "without a baseline". The client transport treats any decode
        // error as the end of the connection.
        if let ServerMessage::PaneSurface(surface) = &message {
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
            // An existing baseline is overwritten in place to keep its cell
            // buffer; only the first surface allocates one.
            let base = self.baseline.get_or_insert_with(|| CellBaseline {
                boot_id: surface.boot_id.clone(),
                projection_revision: ProjectionRevision::default(),
                surface_revision: SurfaceRevision::default(),
                width: 0,
                height: 0,
                cells: Vec::new(),
                meta: None,
            });
            base.boot_id.clone_from(&surface.boot_id);
            base.projection_revision = surface.projection_revision;
            base.surface_revision = surface.surface_revision;
            base.width = surface.frame.width;
            base.height = surface.frame.height;
            base.cells.clone_from(&surface.frame.cells);
            base.meta = Some(surface.into());
        }
        Ok(DecodedServerMessage::Wire(message))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FrameData, PaneSurfacePatchRow, SurfaceUpdate, WireColor, WireStyle};

    fn cell(symbol: &str) -> CellData {
        CellData {
            symbol: symbol.into(),
            grid_width: crate::GridCellWidth::Grapheme,
            fg: WireColor::Reset,
            bg: WireColor::Reset,
            style: WireStyle::default(),
            skip: false,
            hyperlink: None,
        }
    }

    fn surface() -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: "1-1".into(),
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
        let DecodedServerMessage::Wire(ServerMessage::PaneSurface(applied)) = decoder
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
            boot_id: "1-1".into(),
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
        assert_eq!(subject.boot_id, "1-1");
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
            boot_id: "1-1".into(),
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
        let DecodedServerMessage::PaneSurfacePatch(patch) = decoder
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

        // A second baseline kept outside the decoder stays in lockstep by
        // applying the same patch, and refuses to apply it twice.
        let mut kept = surface();
        apply_patch_to_surface(&mut kept, &patch).expect("patch applies to its baseline");
        assert_eq!(Some(kept.clone()), decoder.current_surface());
        assert!(matches!(
            apply_patch_to_surface(&mut kept, &patch),
            Err(SurfaceDecodeError::WithSubject { source, .. })
                if matches!(source.as_ref(), SurfaceDecodeError::PatchBaselineMismatch)
        ));
    }

    #[test]
    fn compact_metadata_round_trips_and_rejected_spans_leave_baseline_unchanged() {
        let mut first = surface();
        first.frame.hyperlinks = vec!["https://example.test/".repeat(4096)];
        let rect = crate::SurfaceRect {
            x: 0,
            y: 0,
            width: 2,
            height: 1,
        };
        first.panes.push(crate::PaneSurfacePane {
            pane_id: "w1:p1".parse().expect("pane ID"),
            content_revision: 1,
            rect,
            inner_rect: rect,
            scrollbar_rect: None,
            scroll: None,
            focused: true,
            mouse_reporting: false,
            sgr_pixel_mouse: false,
            alternate_screen_active: false,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(first.clone()))
            .expect("baseline");
        let mut next = first.clone();
        next.surface_revision = crate::SurfaceRevision::new(2);
        next.panes[0].content_revision = 2;
        next.frame.cursor = Some(crate::CursorState {
            x: 1,
            y: 0,
            visible: true,
            shape: crate::CursorShapeParam::SteadyBar,
        });
        next.frame.cells[0] = cell("z");
        let baseline = Baseline::new(
            &first.boot_id,
            first.projection_revision,
            first.surface_revision,
        );
        let update = baseline.update(
            &next,
            vec![PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![cell("z")],
            }],
            &first,
        );
        assert!(
            matches!(&update.meta, Some(crate::SurfaceMeta::Patch(meta)) if meta.panes.len() == 1)
        );
        let mut bad = update.clone();
        bad.spans[0].cells[0].hyperlink = Some(1);
        assert!(decoder.decode(ServerMessage::SurfaceUpdate(bad)).is_err());
        assert_eq!(decoder.current_surface(), Some(first.clone()));
        let mut bad = update.clone();
        bad.spans.push(bad.spans[0].clone());
        assert!(decoder.decode(ServerMessage::SurfaceUpdate(bad)).is_err());
        assert_eq!(decoder.current_surface(), Some(first));
        let mut bytes = Vec::new();
        crate::write_message(&mut bytes, &ServerMessage::SurfaceUpdate(update)).expect("encode");
        assert!(
            bytes.len() < 1024,
            "retained hyperlink must not cross the wire"
        );
        let wire = crate::read_message(&mut bytes.as_slice()).expect("decode wire");
        assert!(matches!(
            decoder.decode(wire).expect("apply"),
            DecodedServerMessage::PaneSurfacePatch(_)
        ));
        assert_eq!(decoder.current_surface(), Some(next));
    }

    #[test]
    fn projection_update_validates_only_the_resulting_hyperlink_indices() {
        let mut first = surface();
        first.frame.hyperlinks = vec!["https://example.test".into()];
        first.frame.cells[0].hyperlink = Some(0);
        let mut decoder = Decoder::default();
        decoder
            .decode(ServerMessage::PaneSurface(first.clone()))
            .expect("baseline");
        let mut next = first.clone();
        next.projection_revision = crate::ProjectionRevision::new(2);
        next.surface_revision = crate::SurfaceRevision::new(2);
        next.frame.hyperlinks.clear();
        next.frame.cells[0] = cell("z");
        let baseline = Baseline::new(
            &first.boot_id,
            first.projection_revision,
            first.surface_revision,
        );
        let bad = baseline.update(&next, Vec::new(), &first);
        assert!(decoder.decode(ServerMessage::SurfaceUpdate(bad)).is_err());
        assert_eq!(decoder.current_surface(), Some(first.clone()));
        let good = baseline.update(
            &next,
            vec![PaneSurfacePatchRow {
                x: 0,
                y: 0,
                cells: vec![cell("z")],
            }],
            &first,
        );
        decoder
            .decode(ServerMessage::SurfaceUpdate(good))
            .expect("overwritten index is valid");
        assert_eq!(decoder.current_surface(), Some(next));
    }
}
