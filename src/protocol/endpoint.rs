//! JSON handshake and named controls for client-owned shells.
//!
//! Client and server are always the same build; the handshake carries
//! `PROTOCOL_VERSION` so an accidental mismatch fails with a clear error.

use serde::{Deserialize, Serialize};

use super::{ClientShellSnapshot, ClientSurfaceSize, ServerMessage};

pub const ENDPOINT_HELLO_KIND: &str = "endpoint.hello.v1";
pub const ENDPOINT_WELCOME_KIND: &str = "endpoint.welcome.v1";
pub const ENDPOINT_SNAPSHOT_KIND: &str = "shell.snapshot.v1";
pub const SURFACE_INTEREST_CAPABILITY: &str = "surface_interest";
pub const PRESENTATION_EFFECTS_FENCE_CAPABILITY: &str = "presentation_effects_fence";
pub const PRESENTATION_EFFECTS_SYNC_KIND: &str = "endpoint.presentation.sync.v1";
pub const PRESENTATION_EFFECTS_READY_KIND: &str = "endpoint.presentation.ready.v1";
pub const HEALTH_CHECK_CAPABILITY: &str = "health_check";
pub const HEALTH_PING_KIND: &str = "endpoint.health.ping.v1";
pub const HEALTH_PONG_KIND: &str = "endpoint.health.pong.v1";
pub const AGENT_COMPLETIONS_CAPABILITY: &str = "agent_completions";
pub const AGENT_COMPLETIONS_KIND: &str = "endpoint.agent-completions.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointAgentCompletions {
    pub boot_id: String,
    pub revision: u64,
    pub completions: std::collections::BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointClientHello {
    /// Must equal the server's `PROTOCOL_VERSION`.
    pub version: u32,
    pub cell_width_px: u32,
    pub cell_height_px: u32,
    pub surface_size: ClientSurfaceSize,
    pub pixel_mouse: bool,
    pub endpoint_keybindings: bool,
    pub mouse_capture: bool,
    pub surface_active: bool,
    /// Accept the cell-retaining surface encoding on this connection.
    pub surface_reuse: bool,
    /// Accept the surface-delta encoding on this connection.
    pub surface_delta: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointHandshakeError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointServerWelcome {
    /// The server's `PROTOCOL_VERSION`.
    pub version: u32,
    pub methods: Vec<String>,
    pub capabilities: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<EndpointHandshakeError>,
}

pub fn snapshot_message(snapshot: &ClientShellSnapshot) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: ENDPOINT_SNAPSHOT_KIND.into(),
        data: serde_json::to_string(snapshot)?,
    })
}

pub fn agent_completions_message(
    projection: &EndpointAgentCompletions,
) -> serde_json::Result<ServerMessage> {
    Ok(ServerMessage::EndpointControl {
        kind: AGENT_COMPLETIONS_KIND.into(),
        data: serde_json::to_string(projection)?,
    })
}

impl EndpointServerWelcome {
    pub fn compatible(methods: Vec<String>) -> Self {
        Self {
            version: super::PROTOCOL_VERSION,
            methods,
            capabilities: vec![
                super::surface_reuse::CAPABILITY.into(),
                super::surface_delta::CAPABILITY.into(),
                SURFACE_INTEREST_CAPABILITY.into(),
                PRESENTATION_EFFECTS_FENCE_CAPABILITY.into(),
                HEALTH_CHECK_CAPABILITY.into(),
                AGENT_COMPLETIONS_CAPABILITY.into(),
            ],
            error: None,
        }
    }

    pub fn incompatible(code: &str, message: impl Into<String>) -> Self {
        Self {
            version: super::PROTOCOL_VERSION,
            methods: Vec::new(),
            capabilities: Vec::new(),
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
            config_diagnostic: None,
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
        let welcome = EndpointServerWelcome::compatible(vec!["pane.close".into()]);
        let json = serde_json::to_string(&welcome).expect("test precondition");
        let decoded: EndpointServerWelcome =
            serde_json::from_str(&json).expect("test precondition");
        assert_eq!(decoded, welcome);
        assert_eq!(decoded.version, super::super::PROTOCOL_VERSION);
    }
}
