use super::ConnectionRole;
use shepr_protocol::{ServerMessage, surface_reuse::DecodedServerMessage};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationDecision {
    Apply,
    Drop,
    /// Evidence for the move being prepared.
    Buffer,
}

/// Classifies every inbound message by its sender's role. Connection-level messages always
/// apply; frames apply for the shown endpoint and are evidence for the target; host effects
/// and everything else belong to the shown endpoint alone. One role comparison per message.
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
    pub(crate) fn decide(&self, message: &DecodedServerMessage) -> PresentationDecision {
        use ConnectionRole::*;
        use PresentationDecision::*;
        match message {
            DecodedServerMessage::Wire(
                ServerMessage::EndpointWelcome(_)
                | ServerMessage::EndpointSnapshot(_)
                | ServerMessage::HealthPong
                | ServerMessage::ServerShutdown { .. },
            ) => Apply,
            DecodedServerMessage::PaneSurfacePatch(_)
            | DecodedServerMessage::Wire(ServerMessage::PaneSurface(_)) => match self.role {
                Shown => Apply,
                Target => Buffer,
                Other => Drop,
            },
            DecodedServerMessage::Wire(ServerMessage::ClientShellEndpointResponse { .. }) => {
                match self.role {
                    Shown => Apply,
                    Target if self.move_response => Buffer,
                    _ => Drop,
                }
            }
            _ => {
                if self.role == Shown {
                    Apply
                } else {
                    Drop
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ConnectionRole::*;
    use PresentationDecision::*;
    fn gate(role: ConnectionRole) -> PresentationGate {
        PresentationGate::new(role, false)
    }
    fn wire(message: ServerMessage) -> DecodedServerMessage {
        DecodedServerMessage::Wire(message)
    }
    fn response() -> DecodedServerMessage {
        wire(ServerMessage::ClientShellEndpointResponse {
            boot_id: crate::tests::test_boot_id("boot"),
            request_id: "request".into(),
            result: Ok(shepr_protocol::command::EndpointReply::Done),
        })
    }
    #[test]
    fn target_effects_are_dropped_and_shown_effects_apply() {
        for effect in [
            ServerMessage::MouseCapture {
                enabled: true,
                sgr_pixels: false,
            },
            ServerMessage::ClientShellKeyboardReportAll { enabled: true },
            ServerMessage::WindowTitle {
                title: Some("remote".into()),
            },
            ServerMessage::Clipboard {
                data: "text".into(),
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
            projection_revision: 1.into(),
            surface_revision: 1.into(),
            frame: shepr_protocol::FrameData {
                cells: vec![],
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: vec![],
            },
            panes: vec![],
            splits: vec![],
        };
        let patch = shepr_protocol::PaneSurfacePatch {
            boot_id: surface.boot_id.clone(),
            projection_revision: 1.into(),
            base_surface_revision: 1.into(),
            surface_revision: 2.into(),
            rows: vec![],
            panes: vec![],
            cursor: None,
        };
        for message in [
            wire(ServerMessage::PaneSurface(surface)),
            DecodedServerMessage::PaneSurfacePatch(patch),
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
            revision: 1.into(),
            restore_notice: Some(shepr_protocol::SessionRestoreNotice {
                unusable: None,
                dropped_workspaces: 1,
                panes_pruned: false,
                backup_dir: "/state/session-backups".into(),
            }),
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
