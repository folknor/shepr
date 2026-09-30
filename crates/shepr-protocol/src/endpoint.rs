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

/// Client-owned shell welcome: the server's validated config when it accepts
/// the connection, or why it refused.
///
/// The config belongs to the connection the welcome opens: the receiver decodes
/// it here, through `ValidatedConfig::deserialize` with the checks that only
/// mean something on the sending host skipped, and installs it before it
/// processes any snapshot of that connection. A config that does not decode
/// fails the handshake. It is sent once per connection, never per snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EndpointServerWelcome {
    /// Boxed: the config is far larger than every other message's payload.
    Accepted {
        config: Box<shepr_config::ValidatedConfig>,
    },
    Refused(HandshakeRefusal),
}

pub fn snapshot_message(snapshot: &ClientShellSnapshot) -> ServerMessage {
    ServerMessage::EndpointSnapshot(Box::new(snapshot.clone()))
}

impl EndpointServerWelcome {
    pub fn accepted(config: shepr_config::ValidatedConfig) -> Self {
        Self::Accepted {
            config: Box::new(config),
        }
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
            revision: crate::ProjectionRevision::new(1),
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

    /// The default config on absolute paths that survive the received-value
    /// checks. This crate cannot use `shepr-test-fixtures`, which depends on it.
    fn config() -> shepr_config::ValidatedConfig {
        let mut config = shepr_config::Config::default();
        config.terminal.default_shell = "/bin/sh".to_owned();
        let root = std::path::Path::new("/nonexistent/shepr-test-config");
        let paths = shepr_config::AppPaths::rooted_at(root, Some(root), None);
        shepr_config::ValidatedConfig::from_values(config, None, paths)
            .expect("test config is valid")
    }

    #[test]
    fn welcome_carries_the_config_through_the_wire() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted(config()));
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

    #[test]
    fn welcome_with_a_truncated_config_does_not_decode() {
        let welcome = ServerMessage::EndpointWelcome(EndpointServerWelcome::accepted(config()));
        let bytes = crate::encode_message(&welcome).expect("test precondition");
        // Keep the one-byte message and welcome variant indexes and cut the
        // config off, under a length prefix that matches.
        let mut framed = 2u32.to_le_bytes().to_vec();
        framed.extend_from_slice(&bytes[4..6]);
        assert!(crate::read_message::<_, ServerMessage>(&mut framed.as_slice()).is_err());
    }
}
