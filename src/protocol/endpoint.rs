//! JSON handshake and named controls for client-owned shells.
//!
//! Client and server are always the same build; the connection preamble checks
//! that identity before either side decodes this JSON control envelope.

use serde::{Deserialize, Serialize};

use super::{ClientShellSnapshot, ClientSurfaceSize, ServerMessage};

pub const ENDPOINT_HELLO_KIND: &str = "endpoint.hello.v1";
pub const ENDPOINT_WELCOME_KIND: &str = "endpoint.welcome.v1";
pub const ENDPOINT_SNAPSHOT_KIND: &str = "shell.snapshot.v1";
pub const PRESENTATION_EFFECTS_SYNC_KIND: &str = "endpoint.presentation.sync.v1";
pub const PRESENTATION_EFFECTS_READY_KIND: &str = "endpoint.presentation.ready.v1";
pub const HEALTH_PING_KIND: &str = "endpoint.health.ping.v1";
pub const HEALTH_PONG_KIND: &str = "endpoint.health.pong.v1";

/// Client-owned shell hello.
///
/// This hello selects the client-owned-shell mode, which receives semantic
/// surfaces. There is no encoding negotiation: the exact-build preamble
/// guarantees the peer supports every encoding this build sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointClientHello {
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub surface_size: ClientSurfaceSize,
    pub pixel_mouse: bool,
    pub mouse_capture: bool,
    pub surface_active: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointHandshakeError {
    pub code: String,
    pub message: String,
}

/// Client-owned shell welcome: why the server refused, if it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointServerWelcome {
    pub error: Option<EndpointHandshakeError>,
}

pub fn snapshot_message(snapshot: &ClientShellSnapshot) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: ENDPOINT_SNAPSHOT_KIND.into(),
        data: serde_json::to_string(snapshot)?,
    })
}

impl EndpointServerWelcome {
    pub fn compatible() -> Self {
        Self { error: None }
    }

    pub fn incompatible(code: &str, message: impl Into<String>) -> Self {
        Self {
            error: Some(EndpointHandshakeError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ClientShellSnapshot {
        ClientShellSnapshot {
            boot_id: "boot".into(),
            revision: 1,
            server_keybindings_toml: None,
            focused_workspace_id: None,
            focused_tab_id: None,
            focused_pane_id: None,
            tab_bar_right: Vec::new(),
            tab_bar_right_separator: String::new(),
            workspaces: Vec::new(),
            tabs: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn snapshot_message_uses_named_json_control() {
        let snapshot = snapshot();
        let ServerMessage::EndpointControl { kind, data } =
            snapshot_message(&snapshot).expect("test precondition")
        else {
            panic!("snapshot should use endpoint control");
        };
        assert_eq!(kind, ENDPOINT_SNAPSHOT_KIND);
        let decoded: ClientShellSnapshot = serde_json::from_str(&data).expect("test precondition");
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn welcome_roundtrips_through_json() {
        let welcome = EndpointServerWelcome::compatible();
        let json = serde_json::to_string(&welcome).expect("test precondition");
        let decoded: EndpointServerWelcome =
            serde_json::from_str(&json).expect("test precondition");
        assert_eq!(decoded, welcome);
    }
}
