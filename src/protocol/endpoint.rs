//! Typed handshake data for client-owned shells.

use serde::{Deserialize, Serialize};

use super::{ClientShellSnapshot, HandshakeRefusal, ServerMessage};

/// Client-owned shell hello.
///
/// This hello selects the client-owned-shell mode, which receives semantic
/// surfaces. There is no encoding negotiation: the exact-build preamble
/// guarantees the peer supports every encoding this build sends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointClientHello {
    pub geometry: super::TerminalGeometry,
    pub mouse_capture: bool,
    pub surface_active: bool,
}

/// Client-owned shell welcome: why the server refused, if it did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointServerWelcome {
    pub error: Option<HandshakeRefusal>,
}

pub fn snapshot_message(snapshot: &ClientShellSnapshot) -> ServerMessage {
    ServerMessage::EndpointSnapshot(Box::new(snapshot.clone()))
}

impl EndpointServerWelcome {
    pub fn compatible() -> Self {
        Self { error: None }
    }

    pub fn incompatible(reason: HandshakeRefusal) -> Self {
        Self {
            error: Some(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ClientShellSnapshot {
        ClientShellSnapshot {
            boot_id: "boot".into(),
            revision: crate::protocol::ProjectionRevision::new(1),
            resolved_config: vec![1, 2, 3],
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
    fn snapshot_message_carries_typed_snapshot() {
        let snapshot = snapshot();
        let message = snapshot_message(&snapshot);
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, &message).expect("test precondition");
        let decoded: ServerMessage =
            crate::protocol::read_message(&mut bytes.as_slice(), crate::protocol::MAX_FRAME_SIZE)
                .expect("test precondition");
        let ServerMessage::EndpointSnapshot(decoded) = decoded else {
            panic!("snapshot should use typed endpoint message");
        };
        assert_eq!(*decoded, snapshot);
    }

    #[test]
    fn welcome_roundtrips_through_the_wire() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::compatible());
        let mut bytes = Vec::new();
        crate::protocol::write_message(&mut bytes, &welcome).expect("test precondition");
        let decoded: ServerMessage =
            crate::protocol::read_message(&mut bytes.as_slice(), crate::protocol::MAX_FRAME_SIZE)
                .expect("test precondition");
        assert_eq!(decoded, welcome);
    }
}
