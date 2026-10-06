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

/// Accepts or refuses the connection. Each process uses its own launch config;
/// configuration never crosses the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointServerWelcome {
    Accepted,
    Refused(HandshakeRefusal),
}

pub fn snapshot_message(snapshot: &ClientShellSnapshot) -> ServerMessage {
    ServerMessage::EndpointSnapshot(Box::new(snapshot.clone()))
}

impl EndpointServerWelcome {
    pub fn accepted() -> Self {
        Self::Accepted
    }

    pub fn refused(reason: HandshakeRefusal) -> Self {
        Self::Refused(reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ClientShellSnapshot {
        ClientShellSnapshot {
            boot_id: "1-1".into(),
            restore_notice: None,
            session_save_status: crate::SessionSaveStatus::Ready,
            revision: crate::revision::at(1),
            focused_workspace_id: None,
            focused_pane_id: None,
            workspaces: Vec::new(),
            panes: Vec::new(),
            agents: Vec::new(),
        }
    }

    #[test]
    fn snapshot_message_carries_typed_snapshot() {
        let snapshot = snapshot();
        let message = snapshot_message(&snapshot);
        let mut bytes = Vec::new();
        crate::write_message(&mut bytes, &message).expect("test precondition");
        let decoded: ServerMessage =
            crate::read_message(&mut bytes.as_slice()).expect("test precondition");
        let ServerMessage::EndpointSnapshot(decoded) = decoded else {
            panic!("snapshot should use typed endpoint message");
        };
        assert_eq!(*decoded, snapshot);
    }

    #[test]
    fn accepted_welcome_roundtrips() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted());
        let mut bytes = Vec::new();
        crate::write_message(&mut bytes, &welcome).expect("test precondition");
        let decoded: ServerMessage =
            crate::read_message(&mut bytes.as_slice()).expect("test precondition");
        assert_eq!(decoded, welcome);
    }

    #[test]
    fn refusal_roundtrips_through_the_wire() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::refused(
            HandshakeRefusal::ExpectedHello,
        ));
        let mut bytes = Vec::new();
        crate::write_message(&mut bytes, &welcome).expect("test precondition");
        let decoded: ServerMessage =
            crate::read_message(&mut bytes.as_slice()).expect("test precondition");
        assert_eq!(decoded, welcome);
    }
}
