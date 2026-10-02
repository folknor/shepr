use super::*;
use shepr_protocol::{BootId, PaneSurfacePatch, ProjectionRevision};
/// The reader baseline and the last exact snapshot/surface pair have separate roles.
/// Moving a snapshot past its surface copies nothing; only the first patch in that
/// gap splits the two values. Full surfaces and pairing always move the grid.
#[derive(Default)]
pub(super) enum PaneSurfaces {
    #[default]
    /// Nothing received or presented.
    Empty,
    /// Exact snapshot pair, patched and presented in place.
    Paired(PaneSurfaceFrame),
    /// The snapshot passed this pair. It remains both baseline and held presentation.
    Passed(PaneSurfaceFrame),
    /// A lost connection leaves only the held presentation.
    Frozen(PaneSurfaceFrame),
    /// New baseline with a different held presentation, if anything was presented.
    Split {
        baseline: PaneSurfaceFrame,
        held: Option<PaneSurfaceFrame>,
    },
}
pub(super) enum Pairing {
    /// Nothing on screen changed.
    Unchanged,
    /// The baseline is now the presented surface; `previous` is what was presented.
    Presented { previous: Option<PaneSurfaceFrame> },
    /// The snapshot moved past the presented surface: it is held while the baseline advances.
    Passed,
}
/// Why a patch does not follow the baseline. Crate-visible because
/// `ClientPaneSurfacePatchOutcome::Rejected` carries it to `lib.rs`, which logs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PatchRejection {
    NoBaseline,
    /// Boot, projection, base or successor revision differs.
    DoesNotFollow,
    /// Patched pane is absent, or its geometry, focus or pixel size changed.
    PaneGeometry,
    /// Empty row, outside the frame, or outside every patched pane.
    RowOutsideFrame,
}
impl PaneSurfaces {
    /// What is on screen, possibly held while unpaired. This is what input reads.
    pub(super) fn presented(&self) -> Option<&PaneSurfaceFrame> {
        match self {
            Self::Paired(s) | Self::Passed(s) | Self::Frozen(s) => Some(s),
            Self::Split { held, .. } => held.as_ref(),
            Self::Empty => None,
        }
    }
    /// The exact snapshot pair, the only surface `compose` draws.
    pub(super) fn paired(&self) -> Option<&PaneSurfaceFrame> {
        if let Self::Paired(s) = self {
            Some(s)
        } else {
            None
        }
    }
    pub(super) fn is_paired(&self) -> bool {
        matches!(self, Self::Paired(_))
    }
    /// The shown connection's reader baseline, which every patch must follow.
    pub(super) fn baseline(&self) -> Option<&PaneSurfaceFrame> {
        match self {
            Self::Paired(s) | Self::Passed(s) | Self::Split { baseline: s, .. } => Some(s),
            _ => None,
        }
    }
    /// A received surface that differs from what is presented and waits for its snapshot.
    pub(super) fn waiting_baseline(&self) -> Option<&PaneSurfaceFrame> {
        if let Self::Split { baseline, .. } = self {
            Some(baseline)
        } else {
            None
        }
    }
    /// Replaces the baseline with a full surface, keeping what is presented. Never pairs.
    pub(super) fn receive(&mut self, baseline: PaneSurfaceFrame) {
        let held = match std::mem::take(self) {
            Self::Paired(s) | Self::Passed(s) | Self::Frozen(s) => Some(s),
            Self::Split { held, .. } => held,
            Self::Empty => None,
        };
        *self = Self::Split { baseline, held };
    }
    /// Presents the baseline exactly when it has the snapshot's boot and projection
    /// revision; otherwise the last presented pair is held. `Passed` that matches again
    /// is only for totality: a snapshot never moves back within one boot.
    pub(super) fn pair(&mut self, boot: &BootId, revision: ProjectionRevision) -> Pairing {
        let matches =
            |s: &PaneSurfaceFrame| &s.boot_id == boot && s.projection_revision == revision;
        let (next, result) = match std::mem::take(self) {
            Self::Paired(s) if !matches(&s) => (Self::Passed(s), Pairing::Passed),
            Self::Passed(s) if matches(&s) => (Self::Paired(s), Pairing::Unchanged),
            Self::Split { baseline, held } if matches(&baseline) => (
                Self::Paired(baseline),
                Pairing::Presented { previous: held },
            ),
            other => (other, Pairing::Unchanged),
        };
        *self = next;
        result
    }
    /// The connection changed: keep only what is presented, frozen, with no baseline.
    pub(super) fn lose_baseline(&mut self) {
        *self = match std::mem::take(self) {
            Self::Paired(s)
            | Self::Passed(s)
            | Self::Frozen(s)
            | Self::Split { held: Some(s), .. } => Self::Frozen(s),
            _ => Self::Empty,
        };
    }
    /// The one validation per patch, against `baseline()`. Changes nothing.
    pub(super) fn validate(&self, patch: &PaneSurfacePatch) -> Result<(), PatchRejection> {
        let current = self.baseline().ok_or(PatchRejection::NoBaseline)?;
        if patch.boot_id != current.boot_id
            || patch.projection_revision != current.projection_revision
            || patch.base_surface_revision != current.surface_revision
            || current.surface_revision.checked_next() != Some(patch.surface_revision)
        {
            return Err(PatchRejection::DoesNotFollow);
        }
        for updated in &patch.panes {
            let Some(existing) = current
                .panes
                .iter()
                .find(|pane| pane.pane_id == updated.pane_id)
            else {
                return Err(PatchRejection::PaneGeometry);
            };
            if !pane_geometry_matches(existing, updated) {
                return Err(PatchRejection::PaneGeometry);
            }
        }
        for row in &patch.rows {
            if !row_fits_frame(row, &current.frame)
                || row.cells.is_empty()
                || !patch.panes.iter().any(|pane| {
                    let terminal_row = row.x >= pane.inner_rect.x
                        && row.y >= pane.inner_rect.y
                        && row.y < pane.inner_rect.y.saturating_add(pane.inner_rect.height)
                        && row
                            .x
                            .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
                            <= pane.inner_rect.x.saturating_add(pane.inner_rect.width);
                    let scrollbar_rect = pane.scrollbar_rect.or_else(|| {
                        current
                            .panes
                            .iter()
                            .find(|existing| existing.pane_id == pane.pane_id)
                            .and_then(|existing| existing.scrollbar_rect)
                    });
                    let scrollbar_row = scrollbar_rect.is_some_and(|rect| {
                        row.x == rect.x
                            && row.y >= rect.y
                            && row.y < rect.y.saturating_add(rect.height)
                            && row.cells.len() == usize::from(rect.width)
                    });
                    terminal_row || scrollbar_row
                })
            {
                return Err(PatchRejection::RowOutsideFrame);
            }
        }

        Ok(())
    }
    /// Applies a patch `validate` accepted against this unchanged baseline, without
    /// repeating the row and pane checks. `Passed` makes the one grid copy here: the
    /// patched copy becomes the baseline and the passed pair stays held.
    pub(super) fn apply_validated(
        &mut self,
        patch: &PaneSurfacePatch,
    ) -> Result<(), PatchRejection> {
        fn apply(
            surface: &mut PaneSurfaceFrame,
            patch: &PaneSurfacePatch,
        ) -> Result<(), PatchRejection> {
            shepr_protocol::surface_reuse::apply_patch_to_surface(surface, patch)
                .map_err(|_| PatchRejection::DoesNotFollow)
        }
        match std::mem::take(self) {
            Self::Passed(held) => {
                let mut baseline = held.clone();
                let applied = apply(&mut baseline, patch);
                *self = if applied.is_ok() {
                    Self::Split {
                        baseline,
                        held: Some(held),
                    }
                } else {
                    Self::Passed(held)
                };
                applied
            }
            mut other => {
                let applied = match &mut other {
                    Self::Paired(s) | Self::Split { baseline: s, .. } => apply(s, patch),
                    Self::Passed(_) | Self::Frozen(_) | Self::Empty => {
                        Err(PatchRejection::NoBaseline)
                    }
                };
                *self = other;
                applied
            }
        }
    }
}
fn row_fits_frame(row: &shepr_protocol::PaneSurfacePatchRow, frame: &FrameData) -> bool {
    row.x
        .saturating_add(u16::try_from(row.cells.len()).unwrap_or(u16::MAX))
        <= frame.width
        && row.y < frame.height
}

fn pane_geometry_matches(
    left: &shepr_protocol::PaneSurfacePane,
    right: &shepr_protocol::PaneSurfacePane,
) -> bool {
    left.pane_id == right.pane_id
        && left.rect == right.rect
        && left.inner_rect == right.inner_rect
        && left.focused == right.focused
        && left.pixel_width == right.pixel_width
        && left.pixel_height == right.pixel_height
}

#[cfg(test)]
mod tests {
    use super::*;
    fn surface(revision: u64) -> PaneSurfaceFrame {
        crate::tests::endpoint_choice::surface(
            &ClientEndpointId::Local,
            revision,
            ClientSurfaceSize { cols: 20, rows: 10 },
            "cells",
        )
    }
    fn patch(s: &PaneSurfaceFrame) -> PaneSurfacePatch {
        PaneSurfacePatch {
            boot_id: s.boot_id.clone(),
            projection_revision: s.projection_revision,
            base_surface_revision: s.surface_revision,
            surface_revision: s.surface_revision.checked_next().expect("next"),
            rows: vec![],
            panes: vec![],
            cursor: None,
        }
    }
    fn paired() -> PaneSurfaces {
        let s = surface(1);
        let boot = s.boot_id.clone();
        let mut surfaces = PaneSurfaces::default();
        surfaces.receive(s);
        surfaces.pair(&boot, 1.into());
        surfaces
    }
    #[test]
    fn lose_baseline_of_a_split_with_nothing_held_is_empty() {
        let mut s = PaneSurfaces::default();
        s.receive(surface(1));
        s.lose_baseline();
        assert!(matches!(s, PaneSurfaces::Empty));
    }
    #[test]
    fn a_received_surface_becomes_the_baseline_and_pairs_on_an_exact_revision() {
        let mut s = PaneSurfaces::default();
        let frame = surface(1);
        let boot = frame.boot_id.clone();
        s.receive(frame);
        assert!(s.baseline().is_some());
        assert!(s.presented().is_none());
        assert!(matches!(
            s.pair(&boot, 1.into()),
            Pairing::Presented { previous: None }
        ));
        assert!(s.is_paired());
    }
    #[test]
    fn a_surface_ahead_of_the_snapshot_waits_while_the_last_pair_is_held() {
        let mut s = paired();
        s.receive(surface(2));
        s.pair(&surface(1).boot_id, 1.into());
        assert_eq!(s.presented().expect("held").projection_revision, 1);
        assert_eq!(s.baseline().expect("baseline").projection_revision, 2);
    }
    #[test]
    fn a_surface_behind_the_snapshot_waits_for_a_newer_one() {
        let mut s = paired();
        s.receive(surface(2));
        s.pair(&surface(1).boot_id, 3.into());
        assert!(!s.is_paired());
        assert_eq!(s.presented().expect("held").projection_revision, 1);
    }
    #[test]
    fn a_snapshot_moving_past_a_pair_passes_it_without_a_copy() {
        let mut s = paired();
        let ptr = s.baseline().expect("baseline").frame.cells.as_ptr();
        s.pair(&surface(1).boot_id, 2.into());
        assert!(matches!(s, PaneSurfaces::Passed(_)));
        assert_eq!(s.baseline().expect("baseline").frame.cells.as_ptr(), ptr);
    }
    #[test]
    fn the_first_patch_after_a_pass_splits_the_baseline_from_the_held_pair() {
        let mut s = paired();
        s.pair(&surface(1).boot_id, 2.into());
        let p = patch(s.baseline().expect("baseline"));
        s.validate(&p).expect("valid");
        s.apply_validated(&p).expect("apply");
        assert_eq!(s.presented().expect("held").surface_revision, 1);
        assert_eq!(s.baseline().expect("baseline").surface_revision, 2);
    }
    #[test]
    fn a_full_surface_after_a_pass_replaces_the_baseline_without_a_copy() {
        let mut s = paired();
        s.pair(&surface(1).boot_id, 2.into());
        let f = surface(2);
        let ptr = f.frame.cells.as_ptr();
        s.receive(f);
        assert_eq!(s.baseline().expect("baseline").frame.cells.as_ptr(), ptr);
    }
    #[test]
    fn a_different_boot_never_pairs() {
        let mut s = paired();
        s.receive(surface(2));
        s.pair(&crate::tests::test_boot_id("remote-boot"), 2.into());
        assert!(!s.is_paired());
    }
    #[test]
    fn lose_baseline_keeps_only_the_held_pair() {
        let mut s = paired();
        s.receive(surface(2));
        s.lose_baseline();
        assert!(s.baseline().is_none());
        assert_eq!(s.presented().expect("held").surface_revision, 1);
    }
    #[test]
    fn a_patch_without_a_baseline_is_rejected() {
        assert_eq!(
            PaneSurfaces::default().validate(&patch(&surface(1))),
            Err(PatchRejection::NoBaseline)
        );
    }
    #[test]
    fn a_patch_that_does_not_follow_the_baseline_is_rejected_and_changes_nothing() {
        let s = paired();
        assert_eq!(
            s.validate(&patch(&surface(2))),
            Err(PatchRejection::DoesNotFollow)
        );
        assert_eq!(s.baseline().expect("baseline").surface_revision, 1);
    }
    #[test]
    fn a_patch_on_a_waiting_baseline_leaves_the_held_pair_untouched() {
        let mut s = paired();
        s.receive(surface(2));
        let p = patch(s.baseline().expect("baseline"));
        s.validate(&p).expect("valid");
        s.apply_validated(&p).expect("apply");
        assert_eq!(s.presented().expect("held").surface_revision, 1);
        assert_eq!(s.baseline().expect("baseline").surface_revision, 3);
    }
}
