use shepr_protocol::ServerMessage;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PresentationDecision {
    Apply,
    Drop,
    Buffer,
}

/// Decides whether one endpoint message may affect the current presentation, belongs to a
/// pending activation, or must be ignored.
pub(crate) struct PresentationGate {
    endpoint_active: bool,
    presentation_owned: bool,
    activation_pending: bool,
    buffer_surface_evidence: bool,
    command_response: bool,
    frozen: bool,
}

impl PresentationGate {
    pub(crate) fn new(
        endpoint_active: bool,
        presentation_owned: bool,
        activation_pending: bool,
        buffer_surface_evidence: bool,
        command_response: bool,
        frozen: bool,
    ) -> Self {
        Self {
            endpoint_active,
            presentation_owned,
            activation_pending,
            buffer_surface_evidence,
            command_response,
            frozen,
        }
    }

    pub(crate) fn decide(&self, message: &ServerMessage) -> PresentationDecision {
        if matches!(
            message,
            ServerMessage::EndpointWelcome(_)
                | ServerMessage::EndpointSnapshot(_)
                | ServerMessage::PresentationReady(_)
                | ServerMessage::HealthPong
                | ServerMessage::ServerShutdown { .. }
        ) {
            return PresentationDecision::Apply;
        }

        let validated_sync = self.endpoint_active && self.activation_pending && !self.frozen;
        let owns_presentation = self.endpoint_active && self.presentation_owned;
        if is_presentation_effect(message) && !(owns_presentation || validated_sync) {
            return PresentationDecision::Drop;
        }

        match message {
            // Surfaces and patches for the endpoint a handoff is proving go into its evidence,
            // which stays in lockstep with the connection's decoder baseline.
            ServerMessage::PaneSurface(_) | ServerMessage::PaneSurfacePatch(_)
                if self.buffer_surface_evidence =>
            {
                PresentationDecision::Buffer
            }
            ServerMessage::ClientShellEndpointResponse { .. } if self.activation_pending => {
                PresentationDecision::Buffer
            }
            ServerMessage::ClientShellEndpointResponse { .. } if self.command_response => {
                PresentationDecision::Apply
            }
            ServerMessage::PaneSurface(_) | ServerMessage::PaneSurfacePatch(_) if self.frozen => {
                PresentationDecision::Drop
            }
            ServerMessage::PaneSurface(_) | ServerMessage::PaneSurfacePatch(_)
                if self.endpoint_active =>
            {
                PresentationDecision::Apply
            }
            _ if self.endpoint_active => PresentationDecision::Apply,
            _ => PresentationDecision::Drop,
        }
    }
}

/// Host terminal state and clipboard writes belong to the displayed endpoint. The validated
/// synchronization phase is the one exception to committed ownership: it replays effects after
/// the target pair has been checked and before the server's ready fence.
fn is_presentation_effect(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. }
            | ServerMessage::WindowTitle { .. }
            | ServerMessage::Clipboard { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{FrameData, PaneSurfaceFrame, PaneSurfacePatch};

    /// A gate as the client loop builds it: an active endpoint owns the presentation unless a
    /// handoff is in flight.
    fn gate(
        endpoint_active: bool,
        activation_pending: bool,
        command_response: bool,
        frozen: bool,
    ) -> PresentationGate {
        PresentationGate::new(
            endpoint_active,
            endpoint_active && !activation_pending,
            activation_pending,
            activation_pending,
            command_response,
            frozen,
        )
    }

    fn effects() -> [ServerMessage; 4] {
        [
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
        ]
    }

    #[test]
    fn host_effects_need_an_owned_presentation_or_a_validated_sync() {
        let owned = gate(true, false, false, false);
        // Active in the registry, but nothing owns the presentation and no handoff runs.
        let unowned = PresentationGate::new(true, false, false, false, false, false);
        let validated_sync = PresentationGate::new(true, false, true, true, false, false);
        for effect in effects() {
            assert_eq!(owned.decide(&effect), PresentationDecision::Apply);
            assert_eq!(unowned.decide(&effect), PresentationDecision::Drop);
            assert_eq!(validated_sync.decide(&effect), PresentationDecision::Apply);
        }
        assert_eq!(
            unowned.decide(&ServerMessage::PaneSurface(surface())),
            PresentationDecision::Apply
        );
    }

    fn surface() -> PaneSurfaceFrame {
        PaneSurfaceFrame {
            boot_id: crate::tests::test_boot_id("boot"),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(1),
            frame: FrameData {
                cells: Vec::new(),
                width: 0,
                height: 0,
                cursor: None,
                hyperlinks: Vec::new(),
            },
            panes: Vec::new(),
            splits: Vec::new(),
        }
    }

    fn response(request_id: &str) -> ServerMessage {
        ServerMessage::ClientShellEndpointResponse {
            boot_id: crate::tests::test_boot_id("boot"),
            request_id: request_id.into(),
            result: Ok(shepr_protocol::command::EndpointReply::Done),
        }
    }

    fn patch() -> PaneSurfacePatch {
        PaneSurfacePatch {
            boot_id: crate::tests::test_boot_id("boot"),
            projection_revision: shepr_protocol::ProjectionRevision::new(1),
            base_surface_revision: shepr_protocol::SurfaceRevision::new(1),
            surface_revision: shepr_protocol::SurfaceRevision::new(2),
            rows: Vec::new(),
            panes: Vec::new(),
            cursor: None,
        }
    }

    #[test]
    fn inactive_endpoint_control_applies_but_presentation_effects_drop() {
        assert_eq!(
            gate(false, false, false, false).decide(&ServerMessage::HealthPong),
            PresentationDecision::Apply
        );
        assert_eq!(
            gate(false, false, false, false).decide(&ServerMessage::WindowTitle {
                title: Some("remote".into()),
            }),
            PresentationDecision::Drop
        );
        assert_eq!(
            gate(false, false, false, false).decide(&ServerMessage::Clipboard {
                data: "text".into()
            }),
            PresentationDecision::Drop
        );
    }

    #[test]
    fn activation_surfaces_and_responses_are_buffered() {
        assert_eq!(
            gate(false, true, false, false).decide(&ServerMessage::PaneSurface(surface())),
            PresentationDecision::Buffer
        );
        assert_eq!(
            gate(false, true, false, false).decide(&response("surface")),
            PresentationDecision::Buffer
        );
    }

    #[test]
    fn tracked_command_responses_apply_outside_the_active_presentation() {
        assert_eq!(
            gate(false, false, true, false).decide(&response("command")),
            PresentationDecision::Apply
        );
    }

    #[test]
    fn frozen_activation_drops_effects_and_buffers_surface_patches() {
        let frozen = gate(true, true, false, true);
        assert_eq!(
            frozen.decide(&ServerMessage::MouseCapture {
                enabled: true,
                sgr_pixels: false,
            }),
            PresentationDecision::Drop
        );
        assert_eq!(
            frozen.decide(&ServerMessage::PaneSurfacePatch(patch())),
            PresentationDecision::Buffer
        );
        assert_eq!(
            gate(true, false, false, true).decide(&ServerMessage::PaneSurface(surface())),
            PresentationDecision::Drop
        );
    }
}
