use super::*;

/// Internal events for the client event loop.
pub(super) struct ParsedHostInput {
    pub(super) event: shepr_termio::input::raw_input::RawInputEvent,
    pub(super) pixel_mouse: Option<shepr_termio::input::mouse::HostPixels>,
}

pub(super) enum ClientLoopEvent {
    StdinInput(Vec<ParsedHostInput>),
    Resize(shepr_core::geometry::HostGeometry),
    TerminalUnavailable(io::Error),
    ServerMessage {
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
        message: Box<DecodedServerMessage>,
    },
    ServerDisconnected {
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
        error: std::io::Error,
    },
    EndpointSupervisor(endpoint::EndpointSupervisorEvent),
    ActivateEndpoint {
        endpoint_id: endpoint::ClientEndpointId,
        target: Option<shell::ClientEndpointFocusTarget>,
        /// A superseded handoff deliberately starts a fresh target-on epoch even when source and
        /// latest target have the same identity after restoration.
        force: bool,
    },
    Timer,
}
