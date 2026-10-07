use shepr_protocol::PaneSurfaceFrame;

use shepr_protocol::{BootId, PaneSurfacePatch, ProjectionRevision};
/// The connection generation a baseline came from.
type SurfaceGeneration = shepr_protocol::ConnectionGeneration;

/// The reader baseline and the last exact snapshot/surface pair have separate roles.
/// Moving a snapshot past its surface copies nothing; only the first patch in that
/// gap splits the two values. Full surfaces and pairing always move the grid.
///
/// Every baseline carries the connection generation it came from, so a surface is
/// only ever paired with, and patched by, its own connection. That lets a connection
/// send its first surface before its first snapshot: the baseline survives the
/// snapshot's generation change (and a reboot reset) instead of meeting the old
/// connection's snapshot or being dropped. The held presentation is presentation
/// only and carries none.
#[derive(Default)]
pub(in crate::shell) enum PaneSurfaces {
    #[default]
    /// Nothing received or presented.
    Empty,
    /// Exact snapshot pair, patched and presented in place.
    Paired {
        surface: PaneSurfaceFrame,
        generation: SurfaceGeneration,
    },
    /// The snapshot passed this pair. It remains both baseline and held presentation.
    Passed {
        surface: PaneSurfaceFrame,
        generation: SurfaceGeneration,
    },
    /// A lost connection leaves only the held presentation.
    Frozen(PaneSurfaceFrame),
    /// New baseline with a different held presentation, if anything was presented.
    Split {
        baseline: PaneSurfaceFrame,
        generation: SurfaceGeneration,
        held: Option<PaneSurfaceFrame>,
    },
}
pub(in crate::shell) enum Pairing {
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
    /// Patched pane is absent, or its geometry or focus changed.
    PaneGeometry,
    /// Invalid or over-limit spans, or a row touching a pane the patch does not list.
    RowOutsideFrame,
}
impl PaneSurfaces {
    /// What is on screen, possibly held while unpaired. This is what input reads.
    pub(in crate::shell) fn presented(&self) -> Option<&PaneSurfaceFrame> {
        match self {
            Self::Paired { surface: s, .. } | Self::Passed { surface: s, .. } | Self::Frozen(s) => {
                Some(s)
            }
            Self::Split { held, .. } => held.as_ref(),
            Self::Empty => None,
        }
    }
    /// The exact snapshot pair, the only surface `compose_frame` draws.
    pub(in crate::shell) fn paired(&self) -> Option<&PaneSurfaceFrame> {
        if let Self::Paired { surface, .. } = self {
            Some(surface)
        } else {
            None
        }
    }
    pub(super) fn is_paired(&self) -> bool {
        matches!(self, Self::Paired { .. })
    }
    /// The shown connection's reader baseline, which every patch must follow, with
    /// the connection generation it came from.
    fn tagged_baseline(&self) -> Option<(&PaneSurfaceFrame, SurfaceGeneration)> {
        match self {
            Self::Paired {
                surface,
                generation,
            }
            | Self::Passed {
                surface,
                generation,
            }
            | Self::Split {
                baseline: surface,
                generation,
                ..
            } => Some((surface, *generation)),
            Self::Frozen(_) | Self::Empty => None,
        }
    }
    /// The generation of the current baseline, if there is one.
    pub(in crate::shell) fn baseline_generation(&self) -> Option<SurfaceGeneration> {
        self.tagged_baseline().map(|(_, generation)| generation)
    }
    /// A received surface that differs from what is presented and waits for its snapshot.
    pub(in crate::shell) fn waiting_baseline(&self) -> Option<&PaneSurfaceFrame> {
        if let Self::Split { baseline, .. } = self {
            Some(baseline)
        } else {
            None
        }
    }
    /// Replaces the baseline with a full surface from connection `generation`, keeping
    /// what is presented. Never pairs.
    pub(in crate::shell) fn receive(
        &mut self,
        baseline: PaneSurfaceFrame,
        generation: SurfaceGeneration,
    ) {
        let held = match std::mem::take(self) {
            Self::Paired { surface: s, .. } | Self::Passed { surface: s, .. } | Self::Frozen(s) => {
                Some(s)
            }
            Self::Split { held, .. } => held,
            Self::Empty => None,
        };
        *self = Self::Split {
            baseline,
            generation,
            held,
        };
    }
    /// Presents the baseline exactly when it has the snapshot's connection generation,
    /// boot and projection revision; otherwise the last presented pair is held. `Passed`
    /// that matches again is only for totality: a snapshot never moves back within one
    /// boot.
    pub(in crate::shell) fn pair(
        &mut self,
        boot: &BootId,
        revision: ProjectionRevision,
        snapshot_generation: SurfaceGeneration,
    ) -> Pairing {
        let matches = |s: &PaneSurfaceFrame, generation: SurfaceGeneration| {
            generation == snapshot_generation
                && &s.boot_id == boot
                && s.projection_revision == revision
        };
        let (next, result) = match std::mem::take(self) {
            Self::Paired {
                surface,
                generation,
            } if !matches(&surface, generation) => (
                Self::Passed {
                    surface,
                    generation,
                },
                Pairing::Passed,
            ),
            Self::Passed {
                surface,
                generation,
            } if matches(&surface, generation) => (
                Self::Paired {
                    surface,
                    generation,
                },
                Pairing::Unchanged,
            ),
            Self::Split {
                baseline,
                generation,
                held,
            } if matches(&baseline, generation) => (
                Self::Paired {
                    surface: baseline,
                    generation,
                },
                Pairing::Presented { previous: held },
            ),
            other => (other, Pairing::Unchanged),
        };
        *self = next;
        result
    }
    /// The shown snapshot moved to connection `generation`. A baseline that already
    /// came from it (a surface that arrived before its snapshot) stays, with what is
    /// presented held; anything else keeps only the presentation, frozen.
    pub(in crate::shell) fn snapshot_generation_changed(&mut self, generation: SurfaceGeneration) {
        *self = match std::mem::take(self) {
            split @ Self::Split {
                generation: baseline_generation,
                ..
            } if baseline_generation == generation => split,
            Self::Paired { surface: s, .. }
            | Self::Passed { surface: s, .. }
            | Self::Frozen(s)
            | Self::Split { held: Some(s), .. } => Self::Frozen(s),
            Self::Split { held: None, .. } | Self::Empty => Self::Empty,
        };
    }
    /// The shown endpoint rebooted. Nothing presented may survive (pane IDs can be
    /// reused), but a baseline the incoming connection already sent for the new boot
    /// is kept, unpresented, so its next patch still has something to follow.
    pub(in crate::shell) fn reset_for_boot(
        &mut self,
        boot: &BootId,
        generation: SurfaceGeneration,
    ) {
        *self = match std::mem::take(self) {
            Self::Split {
                baseline,
                generation: baseline_generation,
                ..
            } if baseline_generation == generation && &baseline.boot_id == boot => Self::Split {
                baseline,
                generation,
                held: None,
            },
            _ => Self::Empty,
        };
    }
    /// The one validation per patch from connection `generation`, against
    /// `baseline()`. Changes nothing. A patch from another connection than the
    /// baseline's has no baseline to follow.
    pub(super) fn validate(
        &self,
        patch: &PaneSurfacePatch,
        generation: SurfaceGeneration,
    ) -> Result<(), PatchRejection> {
        let (current, baseline_generation) =
            self.tagged_baseline().ok_or(PatchRejection::NoBaseline)?;
        if baseline_generation != generation {
            return Err(PatchRejection::NoBaseline);
        }
        shepr_surface::decode::SurfaceBaseline::new(current)
            .admits(patch)
            .map_err(patch_rejection)
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
            shepr_surface::decode::apply_patch_to_surface(surface, patch)
                .map_err(|_| PatchRejection::DoesNotFollow)
        }
        match std::mem::take(self) {
            Self::Passed {
                surface: held,
                generation,
            } => {
                let mut baseline = held.clone();
                let applied = apply(&mut baseline, patch);
                *self = if applied.is_ok() {
                    Self::Split {
                        baseline,
                        generation,
                        held: Some(held),
                    }
                } else {
                    Self::Passed {
                        surface: held,
                        generation,
                    }
                };
                applied
            }
            mut other => {
                let applied = match &mut other {
                    Self::Paired { surface: s, .. } | Self::Split { baseline: s, .. } => {
                        apply(s, patch)
                    }
                    Self::Passed { .. } | Self::Frozen(_) | Self::Empty => {
                        Err(PatchRejection::NoBaseline)
                    }
                };
                *self = other;
                applied
            }
        }
    }
}
fn patch_rejection(error: shepr_surface::decode::SurfaceDecodeError) -> PatchRejection {
    use shepr_surface::decode::SurfaceDecodeError as DecodeError;

    match error {
        DecodeError::WithSubject { source, .. } => patch_rejection(*source),
        DecodeError::PatchBaselineMismatch | DecodeError::BaselineMismatch => {
            PatchRejection::DoesNotFollow
        }
        DecodeError::MetadataMismatch => PatchRejection::PaneGeometry,
        DecodeError::InvalidRows(_)
        | DecodeError::TooManySpans
        | DecodeError::UnlistedPane
        | DecodeError::InvalidDimensions
        | DecodeError::InvalidCellCount
        | DecodeError::InvalidHyperlink => PatchRejection::RowOutsideFrame,
        _ => PatchRejection::DoesNotFollow,
    }
}

#[cfg(test)]
impl PaneSurfaces {
    /// The shown connection's reader baseline, which every patch must follow.
    pub(super) fn baseline(&self) -> Option<&PaneSurfaceFrame> {
        self.tagged_baseline().map(|(surface, _)| surface)
    }
}

#[cfg(test)]
mod tests {
    use crate::endpoint::ClientEndpointId;

    use super::PaneSurfacePatch;
    use crate::shell::presentation::surfaces::{
        Pairing, PaneSurfaces, PatchRejection, SurfaceGeneration,
    };
    use shepr_protocol::{ClientSurfaceSize, PaneSurfaceFrame};
    fn g(position: u64) -> SurfaceGeneration {
        crate::tests::test_generation(position)
    }
    fn rev(position: u64) -> shepr_protocol::ProjectionRevision {
        shepr_test_fixtures::counter_at(position)
    }
    fn surface_rev(position: u64) -> shepr_protocol::SurfaceRevision {
        shepr_test_fixtures::counter_at(position)
    }
    fn surface(revision: u64) -> PaneSurfaceFrame {
        crate::tests::endpoints::surface(
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
        paired_at(g(1))
    }
    fn paired_at(generation: SurfaceGeneration) -> PaneSurfaces {
        let s = surface(1);
        let boot = s.boot_id.clone();
        let mut surfaces = PaneSurfaces::default();
        surfaces.receive(s, generation);
        surfaces.pair(&boot, rev(1), generation);
        surfaces
    }
    /// The server's delta planner, the client's decoder and this validation together: a
    /// pane entering the alternate screen with pane scrollbars on drops its gutter, so its
    /// content rect widens while the frame, splits and projection revision stay. A sparse
    /// cell change then still plans as an update, and what the decoder hands the shell must
    /// be something the shell accepts: a full surface, never a patch with new geometry.
    #[test]
    fn an_alternate_screen_gutter_change_reaches_the_shell_as_a_full_surface() {
        use shepr_surface::decode::{
            DecodedClientServerMessage, DecodedWireServerMessage, Decoder,
        };
        use shepr_surface::delta::SurfaceDeltaPlan;

        let size = ClientSurfaceSize { cols: 40, rows: 20 };
        let mut last =
            crate::tests::endpoints::surface(&ClientEndpointId::Local, 1, size, "prompt");
        // The primary screen reserves the last column for the scrollbar track.
        last.panes[0].content_rect.width = size.cols - 1;
        let mut next = last.clone();
        next.surface_revision = surface_rev(2);
        next.panes[0].content_rect.width = size.cols;
        next.panes[0].alternate_screen_active = true;
        next.frame.cells_mut()[0].symbol = "A".into();

        let SurfaceDeltaPlan::Compact(update) =
            shepr_surface::delta::message(&last, &next).expect("planning")
        else {
            panic!("a sparse change is planned as an update");
        };
        let mut decoder = Decoder::default();
        decoder
            .decode_client(shepr_protocol::ServerMessage::PaneSurface(last.clone()))
            .expect("baseline");
        let mut shell = PaneSurfaces::default();
        shell.receive(last.clone(), g(1));
        shell.pair(&last.boot_id, rev(1), g(1));

        let full = match decoder
            .decode_client(update)
            .expect("the decoder takes the update")
        {
            DecodedClientServerMessage::Wire(DecodedWireServerMessage::PaneSurface(full)) => full,
            other => panic!("a geometry change must not reach the shell as a patch: {other:?}"),
        };
        assert_eq!(full.panes, next.panes);
        assert_eq!(full.frame, next.frame);
        shell.receive(full, g(1));
        shell.pair(&last.boot_id, rev(1), g(1));

        // A sparse change that keeps the geometry is still a patch, and the shell takes it.
        let mut later = next.clone();
        later.surface_revision = surface_rev(3);
        later.frame.cells_mut()[1].symbol = "B".into();
        let SurfaceDeltaPlan::Compact(update) =
            shepr_surface::delta::message(&next, &later).expect("planning")
        else {
            panic!("a sparse change is planned as an update");
        };
        let DecodedClientServerMessage::PaneSurfacePatch(patch) = decoder
            .decode_client(update)
            .expect("the decoder takes the update")
        else {
            panic!("an unchanged geometry keeps the patch path");
        };
        shell
            .validate(&patch, g(1))
            .expect("the shell accepts a patch the decoder forwards");
    }

    #[test]
    fn a_generation_change_with_nothing_held_and_no_new_baseline_is_empty() {
        let mut s = PaneSurfaces::default();
        s.receive(surface(1), g(1));
        s.snapshot_generation_changed(g(2));
        assert!(matches!(s, PaneSurfaces::Empty));
    }
    #[test]
    fn a_received_surface_becomes_the_baseline_and_pairs_on_an_exact_revision() {
        let mut s = PaneSurfaces::default();
        let frame = surface(1);
        let boot = frame.boot_id.clone();
        s.receive(frame, g(1));
        assert!(s.baseline().is_some());
        assert!(s.presented().is_none());
        assert!(matches!(
            s.pair(&boot, rev(1), g(1)),
            Pairing::Presented { previous: None }
        ));
        assert!(s.is_paired());
    }
    #[test]
    fn a_surface_ahead_of_the_snapshot_waits_while_the_last_pair_is_held() {
        let mut s = paired();
        s.receive(surface(2), g(1));
        s.pair(&surface(1).boot_id, rev(1), g(1));
        assert_eq!(s.presented().expect("held").projection_revision, rev(1));
        assert_eq!(s.baseline().expect("baseline").projection_revision, rev(2));
    }
    #[test]
    fn a_surface_behind_the_snapshot_waits_for_a_newer_one() {
        let mut s = paired();
        s.receive(surface(2), g(1));
        s.pair(&surface(1).boot_id, rev(3), g(1));
        assert!(!s.is_paired());
        assert_eq!(s.presented().expect("held").projection_revision, rev(1));
    }
    #[test]
    fn a_snapshot_moving_past_a_pair_passes_it_without_a_copy() {
        let mut s = paired();
        let ptr = s.baseline().expect("baseline").frame.cells().as_ptr();
        s.pair(&surface(1).boot_id, rev(2), g(1));
        assert!(matches!(s, PaneSurfaces::Passed { .. }));
        assert_eq!(s.baseline().expect("baseline").frame.cells().as_ptr(), ptr);
    }
    #[test]
    fn the_first_patch_after_a_pass_splits_the_baseline_from_the_held_pair() {
        let mut s = paired();
        s.pair(&surface(1).boot_id, rev(2), g(1));
        let p = patch(s.baseline().expect("baseline"));
        s.validate(&p, g(1)).expect("valid");
        s.apply_validated(&p).expect("apply");
        assert_eq!(
            s.presented().expect("held").surface_revision,
            surface_rev(1)
        );
        assert_eq!(
            s.baseline().expect("baseline").surface_revision,
            surface_rev(2)
        );
    }
    #[test]
    fn a_full_surface_after_a_pass_replaces_the_baseline_without_a_copy() {
        let mut s = paired();
        s.pair(&surface(1).boot_id, rev(2), g(1));
        let f = surface(2);
        let ptr = f.frame.cells().as_ptr();
        s.receive(f, g(1));
        assert_eq!(s.baseline().expect("baseline").frame.cells().as_ptr(), ptr);
    }
    #[test]
    fn a_different_boot_never_pairs() {
        let mut s = paired();
        s.receive(surface(2), g(1));
        s.pair(&crate::tests::test_boot_id("remote-boot"), rev(2), g(1));
        assert!(!s.is_paired());
    }
    #[test]
    fn a_generation_change_keeps_only_the_held_pair() {
        let mut s = paired_at(g(1));
        s.receive(surface(2), g(1));
        s.snapshot_generation_changed(g(2));
        assert!(s.baseline().is_none());
        assert_eq!(
            s.presented().expect("held").surface_revision,
            surface_rev(1)
        );
    }
    #[test]
    fn a_surface_from_another_connection_never_pairs_with_this_snapshot() {
        // Same boot and projection revision, different connection: the old
        // connection's snapshot must not present the new connection's surface.
        let mut s = paired_at(g(1));
        s.receive(surface(1), g(2));
        assert!(matches!(
            s.pair(&surface(1).boot_id, rev(1), g(1)),
            Pairing::Unchanged
        ));
        assert!(!s.is_paired());
    }
    #[test]
    fn a_surface_sent_before_its_snapshot_survives_the_generation_change() {
        let mut s = paired_at(g(1));
        s.receive(surface(1), g(2));
        s.snapshot_generation_changed(g(2));
        assert_eq!(s.baseline_generation(), Some(g(2)));
        assert_eq!(
            s.presented().expect("held").surface_revision,
            surface_rev(1)
        );
        assert!(matches!(
            s.pair(&surface(1).boot_id, rev(1), g(2)),
            Pairing::Presented { .. }
        ));
        let p = patch(s.baseline().expect("baseline"));
        s.validate(&p, g(2))
            .expect("the new connection's patch follows");
    }
    #[test]
    fn a_reboot_reset_keeps_the_incoming_connections_baseline_unpresented() {
        let mut s = paired_at(g(1));
        let rebooted = crate::tests::test_boot_id("restarted-local");
        let mut frame = surface(1);
        frame.boot_id = rebooted.clone();
        s.receive(frame, g(2));
        s.reset_for_boot(&rebooted, g(2));
        assert!(s.presented().is_none());
        assert_eq!(s.baseline_generation(), Some(g(2)));
        s.reset_for_boot(&rebooted, g(3));
        assert!(matches!(s, PaneSurfaces::Empty));
    }
    #[test]
    fn a_patch_from_another_connection_has_no_baseline() {
        let s = paired_at(g(1));
        let p = patch(s.baseline().expect("baseline"));
        assert_eq!(s.validate(&p, g(2)), Err(PatchRejection::NoBaseline));
    }
    #[test]
    fn a_patch_may_change_a_panes_pixel_extent() {
        let s = paired();
        let baseline = s.baseline().expect("baseline");
        let mut pane = baseline.panes[0].clone();
        let grid = shepr_core::geometry::GridSize::clamped(
            pane.content_rect.width.max(1),
            pane.content_rect.height.max(1),
        );
        let extent = shepr_core::geometry::PanePixelExtent::new(grid, 640, 480).expect("nonzero");
        assert_ne!(pane.pixel_mouse.extent(), Some(extent));
        pane.pixel_mouse = shepr_term::mouse::PanePixelMouse::new(true, Some(extent));
        let mut p = patch(baseline);
        p.panes = vec![pane];
        assert_eq!(s.validate(&p, g(1)), Ok(()));
    }
    #[test]
    fn a_row_across_a_listed_panes_chrome_is_accepted_and_on_an_unlisted_one_refused() {
        // A full-render delta's span can run from the terminal cells into the
        // scrollbar column and the border, as when scrolling a full-width line.
        let s = paired();
        let baseline = s.baseline().expect("baseline");
        let pane = baseline.panes[0].clone();
        let width = usize::from(baseline.frame.width());
        let start = usize::from(pane.rect.y) * width + usize::from(pane.rect.x);
        let cells = baseline.frame.cells()[start..start + usize::from(pane.rect.width)].to_vec();
        let mut p = patch(baseline);
        p.rows.push(shepr_protocol::PaneSurfacePatchRow {
            x: pane.rect.x,
            y: pane.rect.y,
            cells,
        });
        assert_eq!(
            s.validate(&p, g(1)),
            Err(PatchRejection::RowOutsideFrame),
            "the pane the row touches is not listed"
        );
        p.panes.push(pane);
        assert_eq!(s.validate(&p, g(1)), Ok(()));
    }

    #[test]
    fn a_patch_without_a_baseline_is_rejected() {
        assert_eq!(
            PaneSurfaces::default().validate(&patch(&surface(1)), g(1)),
            Err(PatchRejection::NoBaseline)
        );
    }
    #[test]
    fn a_patch_that_does_not_follow_the_baseline_is_rejected_and_changes_nothing() {
        let s = paired();
        assert_eq!(
            s.validate(&patch(&surface(2)), g(1)),
            Err(PatchRejection::DoesNotFollow)
        );
        assert_eq!(
            s.baseline().expect("baseline").surface_revision,
            surface_rev(1)
        );
    }
    #[test]
    fn a_patch_on_a_waiting_baseline_leaves_the_held_pair_untouched() {
        let mut s = paired();
        s.receive(surface(2), g(1));
        let p = patch(s.baseline().expect("baseline"));
        s.validate(&p, g(1)).expect("valid");
        s.apply_validated(&p).expect("apply");
        assert_eq!(
            s.presented().expect("held").surface_revision,
            surface_rev(1)
        );
        assert_eq!(
            s.baseline().expect("baseline").surface_revision,
            surface_rev(3)
        );
    }
}
