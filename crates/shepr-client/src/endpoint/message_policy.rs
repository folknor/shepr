use super::ConnectionRole;
use shepr_surface::decode::{DecodedClientServerMessage, DecodedWireServerMessage};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationDecision {
    Apply,
    Drop,
    /// Evidence for the move being prepared.
    Buffer,
    /// Apply to endpoint state and retain as evidence for the move being prepared.
    ApplyAndBuffer,
}

/// Classifies every inbound message by its sender's role. Keep the match exhaustive so a new
/// wire message must receive an explicit presentation disposition here.
pub(crate) struct PresentationGate {
    role: ConnectionRole,
    move_response: bool,
}
impl PresentationGate {
    /// `move_response`: the message answers the move's on or focus request. Shown
    /// responses apply; the command lane accepts only its in-flight request.
    pub(crate) fn new(role: ConnectionRole, move_response: bool) -> Self {
        Self {
            role,
            move_response,
        }
    }

    fn surface_decision(&self) -> PresentationDecision {
        match self.role {
            ConnectionRole::Shown => PresentationDecision::Apply,
            ConnectionRole::Target => PresentationDecision::Buffer,
            ConnectionRole::Other => PresentationDecision::Drop,
        }
    }

    pub(crate) fn decide(&self, message: &DecodedClientServerMessage) -> PresentationDecision {
        use PresentationDecision::*;
        match message {
            DecodedClientServerMessage::PaneSurfacePatch(_) => self.surface_decision(),
            DecodedClientServerMessage::Wire(message) => match message {
                DecodedWireServerMessage::ServerShutdown { .. }
                | DecodedWireServerMessage::HealthPong => Apply,
                DecodedWireServerMessage::EndpointSnapshot(_) => match self.role {
                    ConnectionRole::Target => ApplyAndBuffer,
                    ConnectionRole::Shown | ConnectionRole::Other => Apply,
                },
                DecodedWireServerMessage::PaneSurface(_) => self.surface_decision(),
                DecodedWireServerMessage::ClientShellEndpointResponse { .. } => match self.role {
                    ConnectionRole::Shown => Apply,
                    ConnectionRole::Target if self.move_response => Buffer,
                    _ => Drop,
                },
                DecodedWireServerMessage::ClientShellError { .. }
                | DecodedWireServerMessage::Clipboard { .. }
                | DecodedWireServerMessage::MouseCapture { .. }
                | DecodedWireServerMessage::ClientShellKeyboardReportAll { .. } => {
                    match self.role {
                        ConnectionRole::Shown => Apply,
                        ConnectionRole::Target | ConnectionRole::Other => Drop,
                    }
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionRole::*;
    use PresentationDecision::*;
    use shepr_protocol::ServerMessage;
    fn gate(role: ConnectionRole) -> PresentationGate {
        PresentationGate::new(role, false)
    }
    fn wire(message: ServerMessage) -> DecodedClientServerMessage {
        DecodedClientServerMessage::Wire(
            DecodedWireServerMessage::try_from(message).expect("allowed client wire message"),
        )
    }
    fn response() -> DecodedClientServerMessage {
        wire(ServerMessage::ClientShellEndpointResponse {
            boot_id: crate::tests::test_boot_id("boot"),
            request_id: shepr_protocol::RequestId::allocate(),
            result: Ok(shepr_protocol::command::EndpointReply::Done),
        })
    }
    #[test]
    fn target_effects_are_dropped_and_shown_effects_apply() {
        for effect in [
            ServerMessage::MouseCapture {
                mode: shepr_term::mouse::HostMouseCapture::Cells,
            },
            ServerMessage::ClientShellKeyboardReportAll { enabled: true },
            ServerMessage::Clipboard {
                data: b"text".to_vec(),
            },
        ] {
            let effect = wire(effect);
            assert_eq!(gate(Shown).decide(&effect), Apply);
            assert_eq!(gate(Target).decide(&effect), Drop);
            assert_eq!(gate(Other).decide(&effect), Drop);
        }
    }
    #[test]
    fn a_target_surface_is_buffered_and_an_other_surface_is_dropped() {
        let surface = shepr_protocol::PaneSurfaceFrame {
            boot_id: crate::tests::test_boot_id("boot"),
            projection_revision: shepr_protocol::ProjectionRevision::FIRST,
            surface_revision: shepr_protocol::SurfaceRevision::FIRST,
            frame: shepr_protocol::FrameData::blank(1, 1).expect("test frame size is valid"),
            panes: vec![],
            splits: vec![],
        };
        let patch = shepr_protocol::PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: shepr_protocol::ProjectionRevision::FIRST,
            base_surface_revision: shepr_protocol::SurfaceRevision::FIRST,
            surface_revision: shepr_test_fixtures::counter_at(2),
            rows: vec![],
            panes: vec![],
            cursor: None,
        };
        for message in [
            DecodedClientServerMessage::Wire(DecodedWireServerMessage::PaneSurface(surface)),
            DecodedClientServerMessage::PaneSurfacePatch(patch),
        ] {
            assert_eq!(gate(Shown).decide(&message), Apply);
            assert_eq!(gate(Target).decide(&message), Buffer);
            assert_eq!(gate(Other).decide(&message), Drop);
        }
    }
    #[test]
    fn only_a_move_response_is_buffered_and_a_shown_response_applies() {
        let r = response();
        for role in [Target, Other] {
            assert_eq!(gate(role).decide(&r), Drop);
        }
        assert_eq!(PresentationGate::new(Target, true).decide(&r), Buffer);
        assert_eq!(PresentationGate::new(Shown, false).decide(&r), Apply);
        assert_eq!(PresentationGate::new(Other, true).decide(&r), Drop);
    }
    #[test]
    fn any_response_of_the_shown_endpoint_applies() {
        assert_eq!(
            PresentationGate::new(Shown, false).decide(&response()),
            Apply
        );
    }
    #[test]
    fn an_other_restore_snapshot_applies() {
        let snapshot = shepr_protocol::ClientShellSnapshot {
            boot_id: crate::tests::test_boot_id("restored"),
            revision: shepr_protocol::ProjectionRevision::FIRST,
            restore_notice: Some(shepr_protocol::SessionRestoreNotice {
                loss: shepr_protocol::SessionRestoreLoss::Damaged(
                    shepr_protocol::SessionRestoreDamage {
                        renamed_workspaces: 1,
                        ..Default::default()
                    },
                ),
                backup_dir: "/state/session-backups".into(),
            }),
            session_save_status: shepr_protocol::SessionSaveStatus::Ready,
            focused_workspace_id: None,
            focused_pane_id: None,
            workspaces: vec![],
            panes: vec![],
            agents: vec![],
        };
        assert_eq!(
            gate(Other).decide(&wire(ServerMessage::EndpointSnapshot(Box::new(snapshot)))),
            Apply
        );
    }
}
