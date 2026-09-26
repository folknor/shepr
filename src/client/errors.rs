use std::io;

use crate::protocol;

/// Errors that can occur during client operation.
#[derive(Debug)]
pub enum ClientError {
    /// Could not connect to the server's client socket.
    ConnectionFailed(io::Error),
    /// A host terminal write failed while updating terminal modes or output.
    HostTerminal(io::Error),
    /// Server rejected our handshake.
    HandshakeRejected { error: String },
    /// Server shut down.
    ServerShutdown { reason: Option<String> },
    /// Lost connection to the server.
    ConnectionLost(io::Error),
    /// Protocol error (framing, deserialization).
    Protocol(protocol::FramingError),
}

impl ClientError {
    pub(crate) fn display_with_target(
        &self,
        session: &crate::session::SessionId,
        address: &crate::server::socket_paths::ServerAddress,
    ) -> String {
        let message = self.to_string();
        if matches!(
            self,
            Self::ServerShutdown {
                reason: Some(reason)
            } if reason == "detached"
        ) && std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR).is_err()
        {
            format!(
                "{message}\nRun `{}` to reattach",
                address.attach_command(session)
            )
        } else {
            message
        }
    }
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::ConnectionFailed(err) => {
                write!(f, "failed to connect to server: {err}")?;
                write!(
                    f,
                    "\nIs shepr server running? Start it with `shepr server`."
                )
            }
            ClientError::HostTerminal(err) => write!(f, "host terminal error: {err}"),
            ClientError::HandshakeRejected { error } => {
                write!(f, "server rejected handshake: {error}")
            }
            ClientError::ServerShutdown { reason } => {
                match reason.as_deref() {
                    Some("detached") => {
                        if let Ok(reattach_command) =
                            std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR)
                        {
                            write!(f, "detached from remote server")?;
                            write!(f, "\nRun `{reattach_command}` to reattach")?;
                        } else {
                            write!(f, "detached from server")?;
                        }
                    }
                    _ => {
                        write!(f, "server shut down")?;
                        if let Some(reason) = reason {
                            write!(f, ": {reason}")?;
                        }
                    }
                }
                Ok(())
            }
            ClientError::ConnectionLost(err) => {
                if let Ok(reattach_command) = std::env::var(crate::remote::REATTACH_COMMAND_ENV_VAR)
                {
                    write!(f, "lost connection to remote Shepr: {err}")?;
                    write!(
                        f,
                        "\nIf the remote server survived the SSH or network drop, its panes may still be running."
                    )?;
                    write!(f, "\nRun `{reattach_command}` to reattach")
                } else {
                    write!(f, "lost connection to server: {err}")
                }
            }
            ClientError::Protocol(err) => write!(f, "protocol error: {err}"),
        }
    }
}

impl std::error::Error for ClientError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ClientError::ConnectionFailed(err) => Some(err),
            ClientError::HostTerminal(err) => Some(err),
            ClientError::ConnectionLost(err) => Some(err),
            ClientError::Protocol(err) => Some(err),
            _ => None,
        }
    }
}

impl From<protocol::FramingError> for ClientError {
    fn from(err: protocol::FramingError) -> Self {
        match err {
            protocol::FramingError::UnexpectedEof => ClientError::ConnectionLost(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "server closed connection",
            )),
            protocol::FramingError::Io(err) => ClientError::ConnectionLost(err),
            err => ClientError::Protocol(err),
        }
    }
}
