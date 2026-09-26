use super::*;

/// Internal events for the client event loop.
pub(super) struct ParsedHostInput {
    pub(super) raw: Vec<u8>,
    pub(super) event: crate::raw_input::RawInputEvent,
    pub(super) pixel_mouse: Option<crate::input::mouse::HostPixels>,
}

pub(super) enum ClientLoopEvent {
    StdinInput(Vec<ParsedHostInput>),
    Resize(u16, u16, u32, u32, bool),
    TerminalUnavailable(io::Error),
    ServerMessage {
        endpoint_id: endpoint::ClientEndpointId,
        generation: u64,
        message: Box<ServerMessage>,
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
