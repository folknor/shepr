use crate::endpoint;
use shepr_surface::decode::DecodedClientServerMessage;
use std::io;

/// Internal events for the client event loop.
pub(super) struct ParsedHostInput {
    pub(super) event: shepr_termio::input::raw_input::RawInputEvent,
    pub(super) pixel_mouse: Option<shepr_termio::input::mouse::HostPixels>,
}

pub(super) enum ClientLoopEvent {
    Quit,
    StdinInput(Vec<ParsedHostInput>),
    Resize(shepr_core::geometry::HostGeometry),
    TerminalUnavailable(io::Error),
    ServerMessage {
        endpoint_id: endpoint::ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        message: Box<DecodedClientServerMessage>,
    },
    ServerDisconnected {
        endpoint_id: endpoint::ClientEndpointId,
        generation: shepr_protocol::ConnectionGeneration,
        error: std::io::Error,
    },
    EndpointSupervisor(endpoint::EndpointSupervisorEvent),
    Timer,
}
