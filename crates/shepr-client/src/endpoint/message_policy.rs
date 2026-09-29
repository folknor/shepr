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
    activation_pending: bool,
    command_response: bool,
    frozen: bool,
}

impl PresentationGate {
    pub(crate) fn new(
        endpoint_active: bool,
        activation_pending: bool,
        command_response: bool,
        frozen: bool,
    ) -> Self {
        Self {
            endpoint_active,
            activation_pending,
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
                | ServerMessage::Welcome { .. }
        ) {
            return PresentationDecision::Apply;
        }

        if self.frozen && self.activation_pending && is_presentation_effect(message) {
            return PresentationDecision::Drop;
        }

        match message {
            ServerMessage::PaneSurface(_)
            | ServerMessage::ClientShellEndpointResponseChunk { .. }
                if self.activation_pending =>
            {
                PresentationDecision::Buffer
            }
            ServerMessage::ClientShellEndpointResponseChunk { .. } if self.command_response => {
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

/// Host modes and titles belong to the endpoint holding the host presentation lease. During a
/// frozen endpoint switch they are replayed after commit instead of being applied to the source.
fn is_presentation_effect(message: &ServerMessage) -> bool {
    matches!(
        message,
        ServerMessage::MouseCapture { .. }
            | ServerMessage::ClientShellKeyboardReportAll { .. }
            | ServerMessage::WindowTitle { .. }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use shepr_protocol::{FrameData, PaneSurfaceFrame, PaneSurfacePatch};

    fn gate(
        endpoint_active: bool,
        activation_pending: bool,
        command_response: bool,
        frozen: bool,
    ) -> PresentationGate {
        PresentationGate::new(
            endpoint_active,
            activation_pending,
            command_response,
            frozen,
        )
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
            gate(false, true, false, false).decide(
                &ServerMessage::ClientShellEndpointResponseChunk {
                    boot_id: crate::tests::test_boot_id("boot"),
                    request_id: "surface".into(),
                    final_chunk: true,
                    data: Vec::new(),
                }
            ),
            PresentationDecision::Buffer
        );
    }

    #[test]
    fn tracked_command_responses_apply_outside_the_active_presentation() {
        assert_eq!(
            gate(false, false, true, false).decide(
                &ServerMessage::ClientShellEndpointResponseChunk {
                    boot_id: crate::tests::test_boot_id("boot"),
                    request_id: "command".into(),
                    final_chunk: true,
                    data: Vec::new(),
                }
            ),
            PresentationDecision::Apply
        );
    }

    #[test]
    fn frozen_activation_drops_effects_and_patches() {
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
            PresentationDecision::Drop
        );
        assert_eq!(
            gate(true, false, false, true).decide(&ServerMessage::PaneSurface(surface())),
            PresentationDecision::Drop
        );
    }
}
