use super::{ClientEndpointFocusTarget, ClientEndpointId, focus_lane::FocusLane};
use shepr_protocol::{
    BootId, ClientMessage, ClientShellSnapshot, ClientSurfaceSize, PaneSurfaceFrame,
    PaneSurfacePatch, RequestId, TerminalGeometry,
    command::{EndpointError, EndpointReply},
};
use std::time::Instant;

/// Why a prepared move cannot commit. Rendered only at the notice boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MoveFailure {
    Rejected(EndpointError),
    BadSurfaceAcknowledgement,
    UnexpectedFocusResponse,
    BadFocusAcknowledgement,
    LostPair,
    ProjectionUnavailable,
}

impl std::fmt::Display for MoveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(error) => write!(f, "{error}"),
            Self::BadSurfaceAcknowledgement => {
                f.write_str("surface activation returned an invalid acknowledgement")
            }
            Self::UnexpectedFocusResponse => f.write_str("unexpected focus response"),
            Self::BadFocusAcknowledgement => {
                f.write_str("endpoint focus returned an invalid acknowledgement")
            }
            Self::LostPair => f.write_str("endpoint move lost its coherent snapshot/surface pair"),
            Self::ProjectionUnavailable => f.write_str("endpoint projection is unavailable"),
        }
    }
}

impl std::error::Error for MoveFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected(error) => Some(error),
            _ => None,
        }
    }
}

/// The connection a move prepares: a target is only prepared when it is connected and has a
/// snapshot for this generation, so every field is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewLease {
    pub endpoint_id: ClientEndpointId,
    pub generation: u64,
    pub boot_id: BootId,
    /// The revision of the snapshot the move started from; older snapshots are stale.
    pub minimum_revision: shepr_protocol::ProjectionRevision,
}

/// What one inbound message did to a `Preparing`. Never "ready": the reconcile asks
/// `Preparing::ready` once per turn.
#[derive(Debug, PartialEq, Eq)]
pub enum PrepareProgress {
    Pending,
    /// The rejection is now set; the reconcile fails the move.
    Rejected,
    /// Not for this lease (another endpoint, generation or boot) or below its revision.
    Stale,
}

/// The target's latest snapshot and surface, from which a coherent pair is judged.
#[derive(Clone, Debug, Default)]
pub(super) struct ViewEvidence {
    snapshot_revision: Option<shepr_protocol::ProjectionRevision>,
    focused_workspace_id: Option<shepr_protocol::WorkspaceId>,
    focused_pane_id: Option<shepr_protocol::PublicPaneId>,
    // Preparation consumes queued events in stream order. The reader's decoder can
    // already be several events ahead, so its current baseline cannot replace this
    // evidence without carrying an event-specific surface across the queue. Doing
    // that for every patch would clone and queue full cell grids, including hidden
    // panes. Keep this event-ordered baseline; patch admission and application use
    // the protocol's shared implementation, and geometry invalidates it locally.
    surface: Option<PaneSurfaceFrame>,
}

impl ViewEvidence {
    /// Monotonic: an older snapshot never replaces a newer one.
    fn record_snapshot(&mut self, snapshot: &ClientShellSnapshot) {
        if self
            .snapshot_revision
            .is_none_or(|current| snapshot.revision >= current)
        {
            self.snapshot_revision = Some(snapshot.revision);
            self.focused_workspace_id = snapshot.focused_workspace_id;
            self.focused_pane_id = snapshot.focused_pane_id;
        }
    }

    /// Replaces the surface in `(projection_revision, surface_revision)` order.
    fn record_surface(&mut self, surface: PaneSurfaceFrame) {
        let replace = self.surface.as_ref().is_none_or(|current| {
            surface.projection_revision > current.projection_revision
                || (surface.projection_revision == current.projection_revision
                    && surface.surface_revision >= current.surface_revision)
        });
        if replace {
            self.surface = Some(surface);
        }
    }

    fn record_patch(&mut self, patch: &PaneSurfacePatch) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        if shepr_protocol::surface_reuse::apply_patch_to_surface(surface, patch).is_err() {
            // A move can commit only from a full baseline that stayed in lockstep with
            // the connection decoder. A mismatch must wait for another full surface.
            self.surface = None;
        }
    }

    fn invalidate_surface(&mut self) {
        self.surface = None;
    }

    /// The recorded surface when it pairs with the recorded snapshot at the same projection
    /// revision, at or above `minimum_revision`, and is sized for `size`.
    fn coherent_surface(
        &self,
        minimum_revision: shepr_protocol::ProjectionRevision,
        size: ClientSurfaceSize,
    ) -> Option<&PaneSurfaceFrame> {
        self.surface.as_ref().filter(|surface| {
            self.snapshot_revision == Some(surface.projection_revision)
                && surface.projection_revision >= minimum_revision
                && surface.is_sized_for(size)
        })
    }
}

/// A target turned on and collecting a coherent snapshot and surface pair. Pure: every
/// method only updates or reads this value.
#[derive(Debug)]
pub struct Preparing {
    lease: ViewLease,
    view_request: RequestId,
    floor: Option<shepr_protocol::ProjectionRevision>,
    geometry: TerminalGeometry,
    focus_lane: FocusLane,
    evidence: ViewEvidence,
    rejection: Option<MoveFailure>,
    deadline: Instant,
}
impl Preparing {
    pub(super) fn new(
        lease: ViewLease,
        view_request: RequestId,
        geometry: TerminalGeometry,
        focus: Option<ClientEndpointFocusTarget>,
        now: Instant,
    ) -> Self {
        Self {
            lease,
            view_request,
            floor: None,
            geometry,
            focus_lane: FocusLane::new(focus),
            evidence: ViewEvidence::default(),
            rejection: None,
            deadline: now + crate::limits::ENDPOINT_MOVE_TIMEOUT,
        }
    }
    pub fn lease(&self) -> &ViewLease {
        &self.lease
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn rejection(&self) -> Option<&MoveFailure> {
        self.rejection.as_ref()
    }
    fn matches(&self, endpoint: &ClientEndpointId, generation: u64, boot: &BootId) -> bool {
        self.lease.endpoint_id == *endpoint
            && self.lease.generation == generation
            && &self.lease.boot_id == boot
    }
    /// The surface to commit, once the on acknowledgement's floor is known, nothing was
    /// rejected, the focus lane is settled and the evidence holds a coherent pair at or above
    /// the floor that shows the requested navigation. The current size comes from the geometry
    /// recorded with the preparation and updated on each host resize.
    pub fn ready(&self) -> Option<&PaneSurfaceFrame> {
        if self.rejection.is_some() || !self.focus_lane.settled() {
            return None;
        }
        let surface = self
            .evidence
            .coherent_surface(self.floor?, self.geometry.surface_size())?;
        let matches = match &self.focus_lane.desired {
            Some(ClientEndpointFocusTarget::Pane(id)) => {
                self.evidence.focused_pane_id.as_ref() == Some(id)
                    && surface
                        .panes
                        .iter()
                        .any(|pane| pane.focused && &pane.pane_id == id)
            }
            Some(ClientEndpointFocusTarget::Workspace(id)) => {
                self.evidence.focused_workspace_id.as_ref() == Some(id)
            }
            None => true,
        };
        matches.then_some(surface)
    }

    /// Whether a response answers this move: the lease matches and the request is the on
    /// request or the focus lane's in-flight one.
    pub fn accepts_response(
        &self,
        endpoint: &ClientEndpointId,
        generation: u64,
        boot: &BootId,
        request: &RequestId,
    ) -> bool {
        self.matches(endpoint, generation, boot)
            && (request == &self.view_request || self.focus_lane.accepts(request))
    }

    /// The on acknowledgement sets the floor; a focus response settles the lane. Anything
    /// invalid, and any error, is kept as the rejection for the reconcile to fail the move.
    pub fn receive_response(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        boot: &BootId,
        request: &RequestId,
        result: Result<EndpointReply, EndpointError>,
    ) -> PrepareProgress {
        if !self.accepts_response(endpoint, generation, boot, request) {
            return PrepareProgress::Stale;
        }
        let accepted = match result {
            Err(error) => Err(MoveFailure::Rejected(error)),
            Ok(reply) if request == &self.view_request => match reply {
                EndpointReply::ClientShellSurfaceSet {
                    active: true,
                    projection_revision,
                } => {
                    self.floor = Some(projection_revision);
                    Ok(())
                }
                _ => Err(MoveFailure::BadSurfaceAcknowledgement),
            },
            Ok(reply) => self.focus_lane.receive(&reply),
        };
        if let Err(reason) = accepted {
            self.rejection = Some(reason);
        }
        if self.rejection.is_some() {
            PrepareProgress::Rejected
        } else {
            PrepareProgress::Pending
        }
    }
    pub fn receive_snapshot(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        snapshot: &ClientShellSnapshot,
    ) -> PrepareProgress {
        if !self.matches(endpoint, generation, &snapshot.boot_id)
            || snapshot.revision < self.lease.minimum_revision
        {
            return PrepareProgress::Stale;
        }
        self.evidence.record_snapshot(snapshot);
        PrepareProgress::Pending
    }
    pub fn receive_surface(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        surface: PaneSurfaceFrame,
    ) -> PrepareProgress {
        if !self.matches(endpoint, generation, &surface.boot_id)
            || !surface.is_sized_for(self.geometry.surface_size())
        {
            return PrepareProgress::Stale;
        }
        self.evidence.record_surface(surface);
        PrepareProgress::Pending
    }
    pub fn receive_patch(
        &mut self,
        endpoint: &ClientEndpointId,
        generation: u64,
        patch: &PaneSurfacePatch,
    ) -> PrepareProgress {
        if !self.matches(endpoint, generation, &patch.boot_id) {
            return PrepareProgress::Stale;
        }
        self.evidence.record_patch(patch);
        PrepareProgress::Pending
    }

    /// Records a host resize. An unchanged geometry changes nothing (the server sends no new
    /// surface for it, so dropping the evidence would stall the move); a changed one drops
    /// the recorded surface. Returns whether it changed.
    pub fn update_geometry(&mut self, geometry: TerminalGeometry) -> bool {
        if self.geometry == geometry {
            return false;
        }
        self.geometry = geometry;
        self.evidence.invalidate_surface();
        true
    }

    /// Replaces the navigation the move should show; an in-flight request is not joined.
    pub fn retarget_focus(&mut self, focus: Option<ClientEndpointFocusTarget>) {
        self.focus_lane.desired = focus;
    }

    /// The next navigation request for the target, marked in flight, when the lane wants one
    /// and has none in flight.
    pub fn focus_request(&mut self) -> Option<ClientMessage> {
        self.focus_lane.request(&self.lease.boot_id)
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::{geometry, lease, preparing, remote};
    use super::*;
    fn snapshot(revision: u64) -> ClientShellSnapshot {
        ClientShellSnapshot {
            boot_id: lease().boot_id,
            revision: revision.into(),
            restore_notice: None,
            session_saves_stopped: false,
            focused_workspace_id: None,
            focused_pane_id: None,
            workspaces: vec![],
            panes: vec![],
            agents: vec![],
        }
    }
    fn surface(revision: u64) -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: lease().boot_id,
            projection_revision: revision.into(),
            surface_revision: 1.into(),
            frame: shepr_protocol::FrameData {
                cells: vec![],
                width: 80,
                height: 24,
                cursor: None,
                hyperlinks: vec![],
            },
            panes: vec![],
            splits: vec![],
        }
    }
    fn acknowledge(p: &mut Preparing, floor: u64) {
        assert_eq!(
            p.receive_response(
                &remote(),
                7,
                &lease().boot_id,
                &"client-shell-view:1:on".into(),
                Ok(EndpointReply::ClientShellSurfaceSet {
                    active: true,
                    projection_revision: floor.into()
                })
            ),
            PrepareProgress::Pending
        );
    }
    fn pair(p: &mut Preparing, revision: u64) {
        p.receive_snapshot(&remote(), 7, &snapshot(revision));
        p.receive_surface(&remote(), 7, surface(revision));
    }
    #[test]
    fn ready_needs_the_ack_the_focus_and_an_exact_snapshot_surface_pair() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        pair(p, 2);
        assert!(p.ready().is_none());
        acknowledge(p, 2);
        assert!(p.ready().is_some());
        p.receive_snapshot(&remote(), 7, &snapshot(3));
        assert!(p.ready().is_none());
        p.retarget_focus(Some(ClientEndpointFocusTarget::Workspace(
            crate::tests::test_workspace_id("w1"),
        )));
        pair(p, 3);
        assert!(p.ready().is_none());
    }
    #[test]
    fn the_ack_floor_rejects_older_epoch_surfaces() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        pair(p, 2);
        acknowledge(p, 3);
        assert!(p.ready().is_none());
        pair(p, 3);
        assert!(p.ready().is_some());
    }
    #[test]
    fn stale_generation_and_boot_are_not_evidence() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        let mut s = snapshot(2);
        assert_eq!(p.receive_snapshot(&remote(), 8, &s), PrepareProgress::Stale);
        s.boot_id = crate::tests::test_boot_id("old-boot");
        assert_eq!(p.receive_snapshot(&remote(), 7, &s), PrepareProgress::Stale);
        let mut s = surface(2);
        s.boot_id = crate::tests::test_boot_id("old-boot");
        assert_eq!(p.receive_surface(&remote(), 7, s), PrepareProgress::Stale);
        acknowledge(p, 2);
        assert!(p.ready().is_none());
    }
    #[test]
    fn a_response_for_another_boot_is_not_consumed() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        assert_eq!(
            p.receive_response(
                &remote(),
                7,
                &crate::tests::test_boot_id("old-boot"),
                &"client-shell-view:1:on".into(),
                Ok(EndpointReply::Done)
            ),
            PrepareProgress::Stale
        );
        assert!(p.rejection().is_none());
        acknowledge(p, 2);
    }
    #[test]
    fn an_invalid_view_acknowledgement_is_kept_as_the_rejection() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        assert_eq!(
            p.receive_response(
                &remote(),
                7,
                &lease().boot_id,
                &"client-shell-view:1:on".into(),
                Ok(EndpointReply::Done)
            ),
            PrepareProgress::Rejected
        );
        assert_eq!(p.rejection(), Some(&MoveFailure::BadSurfaceAcknowledgement));
    }
    #[test]
    fn a_rejected_preparing_is_never_ready() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        p.receive_response(
            &remote(),
            7,
            &lease().boot_id,
            &"client-shell-view:1:on".into(),
            Err(EndpointError::ShuttingDown),
        );
        acknowledge_after_rejection(p);
        pair(p, 2);
        assert!(p.ready().is_none());
    }
    fn acknowledge_after_rejection(p: &mut Preparing) {
        p.receive_response(
            &remote(),
            7,
            &lease().boot_id,
            &"client-shell-view:1:on".into(),
            Ok(EndpointReply::ClientShellSurfaceSet {
                active: true,
                projection_revision: 2.into(),
            }),
        );
    }
    #[test]
    fn a_surface_of_another_size_is_not_evidence() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        acknowledge(p, 2);
        p.receive_snapshot(&remote(), 7, &snapshot(2));
        let mut s = surface(2);
        s.frame.width = 79;
        assert_eq!(p.receive_surface(&remote(), 7, s), PrepareProgress::Stale);
        assert!(p.ready().is_none());
    }
    #[test]
    fn a_patch_without_a_baseline_waits_for_a_full_surface() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        acknowledge(p, 2);
        p.receive_snapshot(&remote(), 7, &snapshot(2));
        let patch = PaneSurfacePatch {
            boot_id: lease().boot_id,
            projection_revision: 2.into(),
            base_surface_revision: 1.into(),
            surface_revision: 2.into(),
            rows: vec![],
            panes: vec![],
            cursor: None,
        };
        p.receive_patch(&remote(), 7, &patch);
        assert!(p.ready().is_none());
        p.receive_surface(&remote(), 7, surface(2));
        assert!(p.ready().is_some());
    }
    #[test]
    fn a_changed_geometry_drops_the_recorded_surface() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        acknowledge(p, 2);
        pair(p, 2);
        assert!(p.update_geometry(TerminalGeometry::new(81, 24, 8, 16, false)));
        assert!(p.ready().is_none());
    }
    #[test]
    fn an_unchanged_geometry_keeps_the_recorded_surface() {
        let mut c = preparing();
        let p = c.preparing_mut().expect("preparing");
        acknowledge(p, 2);
        pair(p, 2);
        assert!(!p.update_geometry(geometry()));
        assert!(p.ready().is_some());
    }
}
